// Copyright 2024 the JSR authors. All rights reserved. MIT license.
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use deno_ast::LineAndColumnDisplay;
use deno_ast::MediaType;
use deno_ast::ModuleSpecifier;
use deno_ast::ParsedSource;
use deno_ast::SourceRange;
use deno_ast::SourceRangedForSpanned;
use deno_ast::swc::common::Span;
use deno_ast::swc::common::comments::CommentKind;
use deno_doc::DocNodeDef;
use deno_error::JsErrorBox;
use deno_graph::BuildFastCheckTypeGraphOptions;
use deno_graph::BuildOptions;
use deno_graph::GraphKind;
use deno_graph::ModuleGraph;
use deno_graph::WorkspaceFastCheckOption;
use deno_graph::WorkspaceMember;
use deno_graph::analysis::ModuleInfo;
use deno_graph::ast::CapturingModuleAnalyzer;
use deno_graph::ast::DefaultEsParser;
use deno_graph::ast::ParsedSourceStore;
use deno_graph::source::JsrUrlProvider;
use deno_graph::source::LoadError;
use deno_graph::source::LoadOptions;
use deno_graph::source::NullFileSystem;
use deno_graph::source::load_data_url;
use deno_semver::StackString;
use deno_semver::jsr::JsrPackageReqReference;
use deno_semver::npm::NpmPackageReqReference;
use deno_semver::package::PackageNv;
use deno_semver::package::PackageReqReference;
use futures::FutureExt;
use once_cell::sync::Lazy;
use regex::Regex;
use regex::bytes::Regex as BytesRegex;
use tracing::Instrument;
use tracing::instrument;
use url::Url;

use crate::buckets::BucketWithQueue;
use crate::db::DependencyKind;
use crate::db::ExportsMap;
use crate::db::PackageVersionMeta;
use crate::docs::DocNodesByUrl;
use crate::gcs_paths;
use crate::ids::PackageName;
use crate::ids::PackagePath;
use crate::ids::ScopeName;
use crate::ids::Version;
use crate::npm::NpmTarball;
use crate::npm::NpmTarballFiles;
use crate::npm::NpmTarballOptions;
use crate::npm::create_npm_tarball;
use crate::tarball::PublishError;

pub struct PackageAnalysisData {
  pub exports: ExportsMap,
  pub files: HashMap<PackagePath, Vec<u8>>,
}

pub struct PackageAnalysisOutput {
  pub data: PackageAnalysisData,
  pub module_graph_2: HashMap<String, ModuleInfo>,
  pub doc_nodes_json: Bytes,
  pub doc_search_json: serde_json::Value,
  pub dependencies: HashSet<(DependencyKind, PackageReqReference)>,
  pub npm_tarball: NpmTarball,
  pub readme_path: Option<PackagePath>,
  pub meta: PackageVersionMeta,
}

// We have to spawn another tokio runtime, because
// `deno_graph::ModuleGraph::build` is not thread-safe.
#[tokio::main(flavor = "current_thread")]
pub async fn analyze_package(
  span: tracing::Span,
  registry_url: Url,
  scope: ScopeName,
  name: PackageName,
  version: Version,
  config_file: PackagePath,
  data: PackageAnalysisData,
) -> Result<PackageAnalysisOutput, PublishError> {
  analyze_package_inner(registry_url, scope, name, version, config_file, data)
    .instrument(span)
    .await
}

#[instrument(name = "analyze_package", skip(registry_url, data), err)]
async fn analyze_package_inner(
  registry_url: Url,
  scope: ScopeName,
  name: PackageName,
  version: Version,
  config_file: PackagePath,
  data: PackageAnalysisData,
) -> Result<PackageAnalysisOutput, PublishError> {
  let PackageAnalysisData { exports, files } = data;
  let mut roots = vec![];
  let mut main_entrypoint = None;

  for (key, path) in exports.iter() {
    // Path is a relative path (./foo) to the config file.
    // This is always at the root, so it's also relative to the root of the tarball.
    let path = path.strip_prefix('.').unwrap();
    let path = PackagePath::new(path.to_string()).map_err(|error| {
      PublishError::InvalidPath {
        path: path.to_string(),
        error,
      }
    })?;
    if !files.contains_key(&path) {
      return Err(PublishError::ConfigFileExportsInvalid {
        path: Box::new(config_file.clone()),
        invalid_exports: format!(
          "export '{key}' references entrypoint '{path}' which does not exist",
        ),
      });
    }
    let url = Url::parse(&format!("file://{}", path)).unwrap();

    if key == "." {
      main_entrypoint = Some(url.clone());
    }

    roots.push(url);
  }

  let module_analyzer = ModuleAnalyzer::default();

  let workspace_member = WorkspaceMember {
    base: Url::parse("file:///").unwrap(),
    name: StackString::from_string(format!("@{}/{}", scope, name)),
    version: Some(version.0.clone()),
    exports: exports.clone().into_inner(),
  };
  let workspace_members = vec![workspace_member.clone()];
  let mut graph = ModuleGraph::new(GraphKind::All);
  graph
    .build(
      roots.clone(),
      vec![],
      &SyncLoader { files: &files },
      BuildOptions {
        is_dynamic: false,
        module_analyzer: &module_analyzer,
        // todo: use the data in the package for the file system
        file_system: &NullFileSystem,
        jsr_url_provider: &PassthroughJsrUrlProvider,
        passthrough_jsr_specifiers: true,
        resolver: Some(&JsrResolver {
          member: workspace_member,
        }),
        npm_resolver: None,
        reporter: None,
        executor: Default::default(),
        locker: None,
        skip_dynamic_deps: false,
        module_info_cacher: Default::default(),
        unstable_bytes_imports: false,
        unstable_text_imports: false,
      },
    )
    .await;
  graph
    .valid()
    .map_err(|e| PublishError::GraphError(Box::new(e)))?;
  graph.build_fast_check_type_graph(BuildFastCheckTypeGraphOptions {
    fast_check_cache: None,
    fast_check_dts: true,
    jsr_url_provider: &PassthroughJsrUrlProvider,
    es_parser: Some(&module_analyzer.analyzer),
    resolver: Default::default(),
    workspace_fast_check: WorkspaceFastCheckOption::Enabled(&workspace_members),
  });

  let dependencies = collect_dependencies(&graph)?;

  for module in graph.modules() {
    // Check for global type augementation.
    // TODO(ry): this function should iterate through and returned back a
    // collection of errors instead of just the first one. That way we can say
    // everything wrong in one shot instead of the user fixing one error at a
    // time with each publish.
    if let Some(parsed_source) = module_analyzer
      .analyzer
      .get_parsed_source(module.specifier())
    {
      check_for_banned_extensions(&parsed_source)?;
      check_for_banned_syntax(&parsed_source)?;
      check_for_banned_triple_slash_directives(&parsed_source)?;
    }
  }

  let all_fast_check = graph
    .modules()
    .filter_map(|module| {
      if roots.contains(module.specifier()) {
        module.js()
      } else {
        None
      }
    })
    .all(|js| {
      js.maybe_types_dependency.is_some() || js.fast_check_module().is_some()
    });

  let doc_nodes =
    crate::docs::generate_docs(roots, &graph, &module_analyzer.analyzer)
      .map_err(PublishError::DocError)?;

  let module_graph_2 = module_analyzer.take_module_graph_2();
  let npm_tarball = create_npm_tarball(NpmTarballOptions {
    graph: &graph,
    analyzer: &module_analyzer.analyzer,
    registry_url: &registry_url,
    scope: &scope,
    package: &name,
    version: &version,
    exports: &exports,
    files: NpmTarballFiles::WithBytes(&files),
    dependencies: dependencies.iter(),
  })
  .await
  .map_err(PublishError::NpmTarballError)?;

  let (meta, readme_path) = {
    let readme = files
      .iter()
      .find(|file| file.0.case_insensitive().is_readme());

    (
      generate_score(
        main_entrypoint.clone(),
        &doc_nodes,
        &readme,
        all_fast_check,
      ),
      readme.map(|readme| readme.0.clone()),
    )
  };

  let doc_nodes_json = serde_json::to_vec(&doc_nodes).unwrap().into();

  let info = crate::docs::get_docs_info(&exports, None);

  let ctx = crate::docs::get_generate_ctx(
    doc_nodes,
    main_entrypoint,
    info.rewrite_map,
    scope,
    name,
    version,
    true,
    None,
    false,
    crate::db::RuntimeCompat {
      browser: None,
      deno: None,
      node: None,
      workerd: None,
      bun: None,
    },
    registry_url.to_string(),
  );
  let search_index = deno_doc::html::generate_search_index(&ctx);
  let doc_search_json = if let serde_json::Value::Object(mut obj) = search_index
  {
    obj.remove("nodes").unwrap()
  } else {
    unreachable!()
  };

  Ok(PackageAnalysisOutput {
    data: PackageAnalysisData { exports, files },
    module_graph_2,
    doc_nodes_json,
    doc_search_json,
    dependencies,
    npm_tarball,
    readme_path,
    meta,
  })
}

static INDENTED_CODE_BLOCK_RE: Lazy<BytesRegex> =
  Lazy::new(|| BytesRegex::new(r#"\n\s*?\n( {4}|\t)[^\S\n]*\S"#).unwrap());

fn generate_score(
  main_entrypoint: Option<ModuleSpecifier>,
  doc_nodes_by_url: &DocNodesByUrl,
  readme: &Option<(&PackagePath, &Vec<u8>)>,
  all_fast_check: bool,
) -> PackageVersionMeta {
  let main_entrypoint_doc =
    main_entrypoint.as_ref().and_then(|main_entrypoint| {
      doc_nodes_by_url
        .get(main_entrypoint)
        .unwrap()
        .iter()
        .find(|node| matches!(node.def, DocNodeDef::ModuleDoc))
        .map(|node| &node.js_doc)
    });

  let has_readme_examples = readme.is_some_and(|(_, readme)| {
    readme
      .windows(3)
      .any(|chars| chars == b"```" || chars == b"~~~")
      || INDENTED_CODE_BLOCK_RE.is_match(readme)
  }) || main_entrypoint_doc.is_some_and(|js_doc| {
    js_doc
      .doc
      .as_ref()
      .is_some_and(|doc| doc.contains("```") || doc.contains("~~~"))
      || js_doc
        .tags
        .iter()
        .any(|tag| matches!(tag, deno_doc::js_doc::JsDocTag::Example { .. }))
  });

  PackageVersionMeta {
    has_readme: readme.is_some()
      || main_entrypoint_doc
        .is_some_and(|doc| doc.doc.as_ref().is_some_and(|doc| !doc.is_empty())),
    has_readme_examples,
    all_entrypoints_docs: all_entrypoints_have_module_doc(
      doc_nodes_by_url,
      main_entrypoint,
      readme.is_some(),
    ),
    percentage_documented_symbols: percentage_of_symbols_with_docs(
      doc_nodes_by_url,
    ),
    all_fast_check,
    has_provenance: false, // Provenance score is updated after version publish
  }
}

fn all_entrypoints_have_module_doc(
  doc_nodes_by_url: &DocNodesByUrl,
  main_entrypoint: Option<ModuleSpecifier>,
  has_readme: bool,
) -> bool {
  'modules: for (specifier, nodes) in doc_nodes_by_url {
    for node in nodes {
      if matches!(node.def, DocNodeDef::ModuleDoc) {
        continue 'modules;
      }
    }

    if main_entrypoint
      .as_ref()
      .is_some_and(|main_entrypoint| main_entrypoint == specifier)
      && has_readme
    {
      continue 'modules;
    }

    return false;
  }

  true
}

fn percentage_of_symbols_with_docs(doc_nodes_by_url: &DocNodesByUrl) -> f32 {
  let mut total_symbols = 0;
  let mut documented_symbols = 0;

  for (_specifier, nodes) in doc_nodes_by_url {
    for node in nodes {
      if matches!(node.def, DocNodeDef::ModuleDoc | DocNodeDef::Import { .. })
        || node.declaration_kind == deno_doc::node::DeclarationKind::Private
      {
        continue;
      }

      total_symbols += 1;

      if !node.js_doc.is_empty() {
        documented_symbols += 1;
      }
    }
  }

  if total_symbols == 0 {
    return 1.0;
  }

  (documented_symbols as f32) / (total_symbols as f32)
}

pub struct PassthroughJsrUrlProvider;

impl JsrUrlProvider for PassthroughJsrUrlProvider {
  fn url(&self) -> &Url {
    unreachable!(
      "BuildOptions::passthrough_jsr_specifiers should be set to true"
    )
  }

  fn package_url(&self, _nv: &PackageNv) -> Url {
    unreachable!(
      "BuildOptions::passthrough_jsr_specifiers should be set to true"
    )
  }

  fn package_url_to_nv(&self, _url: &Url) -> Option<PackageNv> {
    None
  }
}

#[derive(Debug)]
pub struct JsrResolver {
  pub member: WorkspaceMember,
}

impl deno_graph::source::Resolver for JsrResolver {
  fn resolve(
    &self,
    specifier_text: &str,
    referrer_range: &deno_graph::Range,
    _kind: deno_graph::source::ResolutionKind,
  ) -> Result<ModuleSpecifier, deno_graph::source::ResolveError> {
    if let Ok(package_ref) = JsrPackageReqReference::from_str(specifier_text) {
      if self.member.name == package_ref.req().name
        && self
          .member
          .version
          .as_ref()
          .map(|v| package_ref.req().version_req.matches(v))
          .unwrap_or(true)
      {
        let export_name = package_ref.sub_path().unwrap_or(".");
        let Some(export) = self.member.exports.get(export_name) else {
          return Err(deno_graph::source::ResolveError::Other(
            JsErrorBox::generic(format!(
              "export '{}' not found in jsr:{}",
              export_name, self.member.name
            )),
          ));
        };
        return Ok(self.member.base.join(export).unwrap());
      }
    }

    Ok(deno_graph::resolve_import(
      specifier_text,
      &referrer_range.specifier,
    )?)
  }
}

struct SyncLoader<'a> {
  files: &'a HashMap<PackagePath, Vec<u8>>,
}

impl SyncLoader<'_> {
  fn load_sync(
    &self,
    specifier: &ModuleSpecifier,
  ) -> deno_graph::source::LoadResult {
    match specifier.scheme() {
      "file" => {
        let Ok(path) = PackagePath::new(specifier.path().to_string()) else {
          return Ok(None);
        };
        let Some(bytes) = self.files.get(&path).cloned() else {
          return Ok(None);
        };
        Ok(Some(deno_graph::source::LoadResponse::Module {
          content: bytes.into(),
          mtime: None,
          specifier: specifier.clone(),
          maybe_headers: None,
        }))
      }
      "http" | "https" | "node" | "npm" | "jsr" | "bun" | "virtual"
      | "cloudflare" => Ok(Some(deno_graph::source::LoadResponse::External {
        specifier: specifier.clone(),
      })),
      "data" => load_data_url(specifier)
        .map_err(|e| LoadError::Other(Arc::new(JsErrorBox::from_err(e)))),
      _ => Ok(None),
    }
  }
}

impl deno_graph::source::Loader for SyncLoader<'_> {
  fn load(
    &self,
    specifier: &ModuleSpecifier,
    _options: LoadOptions,
  ) -> deno_graph::source::LoadFuture {
    let result = self.load_sync(specifier);
    async move { result }.boxed()
  }
}

pub struct RebuildNpmTarballData {
  pub scope: ScopeName,
  pub name: PackageName,
  pub version: Version,
  pub exports: ExportsMap,
  pub files: HashSet<PackagePath>,
  pub dependencies: Vec<(DependencyKind, PackageReqReference)>,
}

// We have to spawn another tokio runtime, because
// `deno_graph::ModuleGraph::build` is not thread-safe.
#[tokio::main(flavor = "current_thread")]
pub async fn rebuild_npm_tarball(
  span: tracing::Span,
  registry_url: Url,
  modules_bucket: BucketWithQueue,
  data: RebuildNpmTarballData,
) -> Result<NpmTarball, anyhow::Error> {
  rebuild_npm_tarball_inner(registry_url, modules_bucket, data)
    .instrument(span)
    .await
}

#[instrument(
  name = "rebuild_npm_tarball",
  skip(registry_url, modules_bucket, data),
  err
)]
async fn rebuild_npm_tarball_inner(
  registry_url: Url,
  modules_bucket: BucketWithQueue,
  data: RebuildNpmTarballData,
) -> Result<NpmTarball, anyhow::Error> {
  let RebuildNpmTarballData {
    scope,
    name,
    version,
    exports,
    files,
    dependencies,
  } = data;

  let mut roots = vec![];
  for (_, path) in exports.iter() {
    // Path is a relative path (./foo) to config file. This is always at the root,
    // so it's also relative to the root of the tarball.
    let path = path.strip_prefix('.').unwrap();
    let path = PackagePath::new(path.to_string()).map_err(|error| {
      PublishError::InvalidPath {
        path: path.to_string(),
        error,
      }
    })?;
    let url = Url::parse(&format!("file://{}", path)).unwrap();
    roots.push(url);
  }

  let module_analyzer = ModuleAnalyzer::default();

  let mut graph = deno_graph::ModuleGraph::new(GraphKind::All);
  let workspace_member = WorkspaceMember {
    base: Url::parse("file:///").unwrap(),
    name: StackString::from_string(format!("@{}/{}", scope, name)),
    version: Some(version.0.clone()),
    exports: exports.clone().into_inner(),
  };
  let workspace_members = vec![workspace_member.clone()];
  graph
    .build(
      roots.clone(),
      vec![],
      &GcsLoader {
        files: &files,
        bucket: &modules_bucket,
        scope: &scope,
        name: &name,
        version: &version,
      },
      BuildOptions {
        is_dynamic: false,
        module_analyzer: &module_analyzer,
        // todo: use the data in the package for the file system
        file_system: &NullFileSystem,
        jsr_url_provider: &PassthroughJsrUrlProvider,
        passthrough_jsr_specifiers: true,
        resolver: Some(&JsrResolver {
          member: workspace_member,
        }),
        npm_resolver: Default::default(),
        reporter: Default::default(),
        executor: Default::default(),
        locker: None,
        skip_dynamic_deps: false,
        module_info_cacher: Default::default(),
        unstable_bytes_imports: false,
        unstable_text_imports: false,
      },
    )
    .await;
  graph.valid()?;
  graph.build_fast_check_type_graph(BuildFastCheckTypeGraphOptions {
    fast_check_cache: Default::default(),
    fast_check_dts: true,
    jsr_url_provider: &PassthroughJsrUrlProvider,
    es_parser: Some(&module_analyzer.analyzer),
    resolver: None,
    workspace_fast_check: WorkspaceFastCheckOption::Enabled(&workspace_members),
  });

  let npm_tarball = create_npm_tarball(NpmTarballOptions {
    graph: &graph,
    analyzer: &module_analyzer.analyzer,
    registry_url: &registry_url,
    scope: &scope,
    package: &name,
    version: &version,
    exports: &exports,
    files: NpmTarballFiles::FromBucket {
      files: &files,
      modules_bucket: &modules_bucket,
    },
    dependencies: dependencies.iter(),
  })
  .await?;

  Ok(npm_tarball)
}

struct GcsLoader<'a> {
  files: &'a HashSet<PackagePath>,
  bucket: &'a BucketWithQueue,
  scope: &'a ScopeName,
  name: &'a PackageName,
  version: &'a Version,
}

impl GcsLoader<'_> {
  fn load_inner(
    &self,
    specifier: &ModuleSpecifier,
  ) -> deno_graph::source::LoadFuture {
    let specifier = specifier.clone();
    match specifier.scheme() {
      "file" => {
        let Ok(path) = PackagePath::new(specifier.path().to_string()) else {
          return async move { Ok(None) }.boxed();
        };
        if !self.files.contains(&path) {
          return async move { Ok(None) }.boxed();
        };
        let gcs_path =
          gcs_paths::file_path(self.scope, self.name, self.version, &path);
        let bucket = self.bucket.clone();
        async move {
          let Some(bytes) = bucket
            .download(gcs_path.into())
            .await
            .map_err(|e| LoadError::Other(Arc::new(JsErrorBox::from_err(e))))?
          else {
            return Ok(None);
          };
          Ok(Some(deno_graph::source::LoadResponse::Module {
            content: bytes.to_vec().into(),
            mtime: None,
            specifier,
            maybe_headers: None,
          }))
        }
        .boxed()
      }
      "http" | "https" | "node" | "npm" | "jsr" | "bun" => async move {
        Ok(Some(deno_graph::source::LoadResponse::External {
          specifier,
        }))
      }
      .boxed(),
      "data" => async move {
        load_data_url(&specifier)
          .map_err(|e| LoadError::Other(Arc::new(JsErrorBox::from_err(e))))
      }
      .boxed(),
      _ => async move { Ok(None) }.boxed(),
    }
  }
}

impl deno_graph::source::Loader for GcsLoader<'_> {
  fn load(
    &self,
    specifier: &ModuleSpecifier,
    _options: LoadOptions,
  ) -> deno_graph::source::LoadFuture {
    self.load_inner(specifier)
  }
}

#[derive(Default)]
pub struct ModuleParser(DefaultEsParser);

impl deno_graph::ast::EsParser for ModuleParser {
  fn parse_program(
    &self,
    options: deno_graph::ast::ParseOptions,
  ) -> Result<ParsedSource, deno_ast::ParseDiagnostic> {
    let source = self.0.parse_program(options)?;
    if let Some(err) = source.diagnostics().first() {
      return Err(err.clone());
    }
    Ok(source)
  }
}

pub struct ModuleAnalyzer {
  pub analyzer: CapturingModuleAnalyzer,
  pub module_info: RefCell<HashMap<Url, ModuleInfo>>,
}

impl Default for ModuleAnalyzer {
  fn default() -> Self {
    Self {
      analyzer: CapturingModuleAnalyzer::new(
        Some(Box::new(ModuleParser::default())),
        None,
      ),
      module_info: Default::default(),
    }
  }
}

impl ModuleAnalyzer {
  fn take_module_graph_2(&self) -> HashMap<String, ModuleInfo> {
    std::mem::take(&mut *self.module_info.borrow_mut())
      .into_iter()
      .filter_map(|(url, info)| {
        if url.scheme() == "file" {
          let path = url.path();
          Some((path.to_string(), info))
        } else {
          None
        }
      })
      .collect()
  }
}

#[async_trait::async_trait(?Send)]
impl deno_graph::analysis::ModuleAnalyzer for ModuleAnalyzer {
  async fn analyze(
    &self,
    specifier: &ModuleSpecifier,
    source: Arc<str>,
    media_type: MediaType,
  ) -> Result<ModuleInfo, JsErrorBox> {
    let module_info =
      self.analyzer.analyze(specifier, source, media_type).await?;
    self
      .module_info
      .borrow_mut()
      .insert(specifier.clone(), module_info.clone());
    Ok(module_info)
  }
}

fn collect_dependencies(
  graph: &ModuleGraph,
) -> Result<HashSet<(DependencyKind, PackageReqReference)>, PublishError> {
  let mut dependencies = HashSet::new();

  for module in graph.modules() {
    match module.specifier().scheme() {
      "npm" => {
        let res = NpmPackageReqReference::from_str(module.specifier().as_str());
        match res {
          Ok(req) => {
            if req.req().version_req.version_text() == "*" {
              return Err(PublishError::NpmMissingConstraint(req));
            } else {
              dependencies.insert((DependencyKind::Npm, req.into_inner()));
            }
          }
          Err(err) => {
            return Err(PublishError::InvalidNpmSpecifier(err));
          }
        }
      }
      "jsr" => {
        let res = JsrPackageReqReference::from_str(module.specifier().as_str());
        match res {
          Ok(req) => {
            if req.req().version_req.version_text() == "*" {
              return Err(PublishError::JsrMissingConstraint(req));
            } else {
              dependencies.insert((DependencyKind::Jsr, req.into_inner()));
            }
          }
          Err(err) => {
            return Err(PublishError::InvalidJsrSpecifier(err));
          }
        }
      }
      "file" | "data" | "node" | "bun" | "virtual" | "cloudflare" => {}
      "http" | "https" => {
        return Err(PublishError::InvalidExternalImport {
          specifier: module.specifier().to_string(),
          info: "http(s) import".to_string(),
        });
      }
      _ => {
        return Err(PublishError::InvalidExternalImport {
          specifier: module.specifier().to_string(),
          info: "unsupported scheme".to_string(),
        });
      }
    }
  }

  Ok(dependencies)
}

fn check_for_banned_extensions(
  parsed_source: &ParsedSource,
) -> Result<(), PublishError> {
  match parsed_source.media_type() {
    deno_ast::MediaType::Cjs | deno_ast::MediaType::Cts => {
      Err(PublishError::CommonJs {
        specifier: parsed_source.specifier().to_string(),
        line: 0,
        column: 0,
      })
    }
    _ => Ok(()),
  }
}

fn check_for_banned_syntax(
  parsed_source: &ParsedSource,
) -> Result<(), PublishError> {
  use deno_ast::swc::ast;

  let line_col = |range: &SourceRange| -> (usize, usize) {
    let LineAndColumnDisplay {
      line_number,
      column_number,
    } = parsed_source
      .text_info_lazy()
      .line_and_column_display(range.start);
    (line_number, column_number)
  };

  for i in parsed_source.program_ref().body() {
    match i {
      deno_ast::ModuleItemRef::ModuleDecl(n) => match n {
        ast::ModuleDecl::TsNamespaceExport(n) => {
          let (line, column) = line_col(&n.range());
          return Err(PublishError::GlobalTypeAugmentation {
            specifier: parsed_source.specifier().to_string(),
            line,
            column,
          });
        }
        ast::ModuleDecl::TsExportAssignment(n) => {
          let (line, column) = line_col(&n.range());
          return Err(PublishError::GlobalTypeAugmentation {
            specifier: parsed_source.specifier().to_string(),
            line,
            column,
          });
        }
        ast::ModuleDecl::TsImportEquals(n) => match n.module_ref {
          ast::TsModuleRef::TsExternalModuleRef(_) => {
            let (line, column) = line_col(&n.range());
            return Err(PublishError::CommonJs {
              specifier: parsed_source.specifier().to_string(),
              line,
              column,
            });
          }
          _ => {
            continue;
          }
        },
        ast::ModuleDecl::Import(n) => {
          if let Some(with) = &n.with {
            let range = Span::new(n.src.span.hi(), with.span.lo()).range();
            let keyword = parsed_source.text_info_lazy().range_text(&range);
            if keyword.contains("assert") {
              let (line, column) = line_col(&with.span.range());
              return Err(PublishError::BannedImportAssertion {
                specifier: parsed_source.specifier().to_string(),
                line,
                column,
              });
            }
          }
        }
        ast::ModuleDecl::ExportNamed(n) => {
          if let Some(with) = &n.with {
            let src = n.src.as_ref().unwrap();
            let range = Span::new(src.span.hi(), with.span.lo()).range();
            let keyword = parsed_source.text_info_lazy().range_text(&range);
            if keyword.contains("assert") {
              let (line, column) = line_col(&with.span.range());
              return Err(PublishError::BannedImportAssertion {
                specifier: parsed_source.specifier().to_string(),
                line,
                column,
              });
            }
          }
        }
        ast::ModuleDecl::ExportAll(n) => {
          if let Some(with) = &n.with {
            let range = Span::new(n.src.span.hi(), with.span.lo()).range();
            let keyword = parsed_source.text_info_lazy().range_text(&range);
            if keyword.contains("assert") {
              let (line, column) = line_col(&with.span.range());
              return Err(PublishError::BannedImportAssertion {
                specifier: parsed_source.specifier().to_string(),
                line,
                column,
              });
            }
          }
        }
        _ => continue,
      },
      deno_ast::ModuleItemRef::Stmt(n) => match n {
        ast::Stmt::Decl(ast::Decl::TsModule(n)) => {
          if n.global {
            let (line, column) = line_col(&n.range());
            return Err(PublishError::GlobalTypeAugmentation {
              specifier: parsed_source.specifier().to_string(),
              line,
              column,
            });
          }
          match &n.id {
            ast::TsModuleName::Str(n) => {
              let (line, column) = line_col(&n.range());
              return Err(PublishError::GlobalTypeAugmentation {
                specifier: parsed_source.specifier().to_string(),
                line,
                column,
              });
            }
            _ => continue,
          }
        }
        _ => continue,
      },
    }
  }
  Ok(())
}

static TRIPLE_SLASH_RE: Lazy<Regex> = Lazy::new(|| {
  Regex::new(
    r#"^/\s+<reference\s+(no-default-lib\s*=\s*"true"|lib\s*=\s*("[^"]+"|'[^']+'))\s*/>\s*$"#,
  )
  .unwrap()
});

fn check_for_banned_triple_slash_directives(
  parsed_source: &ParsedSource,
) -> Result<(), PublishError> {
  let Some(comments) = parsed_source.get_leading_comments() else {
    return Ok(());
  };
  for comment in comments {
    if comment.kind != CommentKind::Line {
      continue;
    }
    if TRIPLE_SLASH_RE.is_match(&comment.text) {
      let lc = parsed_source
        .text_info_lazy()
        .line_and_column_display(comment.range().start);
      return Err(PublishError::BannedTripleSlashDirectives {
        specifier: parsed_source.specifier().to_string(),
        line: lc.line_number,
        column: lc.column_number,
      });
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  fn parse(source: &str) -> deno_ast::ParsedSource {
    let media_type = deno_ast::MediaType::TypeScript;
    parse_with_media_type(source, media_type)
  }

  fn parse_with_media_type(
    source: &str,
    media_type: deno_ast::MediaType,
  ) -> deno_ast::ParsedSource {
    let specifier = deno_ast::ModuleSpecifier::parse("file:///mod.ts").unwrap();
    deno_ast::parse_module(deno_ast::ParseParams {
      specifier,
      text: source.into(),
      media_type,
      capture_tokens: false,
      scope_analysis: false,
      maybe_syntax: None,
    })
    .unwrap()
  }

  #[test]
  fn banned_extensions() {
    let x =
      parse_with_media_type("let x = 1;", deno_ast::MediaType::TypeScript);
    assert!(super::check_for_banned_extensions(&x).is_ok());

    let x = parse_with_media_type("let x = 1;", deno_ast::MediaType::Cjs);
    let err = super::check_for_banned_extensions(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::CommonJs { .. }),
      "{err:?}",
    );

    let x = parse_with_media_type("let x = 1;", deno_ast::MediaType::Cts);
    let err = super::check_for_banned_extensions(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::CommonJs { .. }),
      "{err:?}",
    );
  }

  #[test]
  fn banned_triple_slash_directives() {
    let x = parse("let x = 1;");
    assert!(super::check_for_banned_triple_slash_directives(&x).is_ok());

    let x = parse("/// <reference lib=\"dom\" />");
    let err = super::check_for_banned_triple_slash_directives(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedTripleSlashDirectives { .. }),
      "{err:?}",
    );

    let x = parse("/// <reference no-default-lib=\"true\" />");
    let err = super::check_for_banned_triple_slash_directives(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedTripleSlashDirectives { .. }),
      "{err:?}",
    );

    let x = parse("///   <reference   no-default-lib=\"true\"/>");
    let err = super::check_for_banned_triple_slash_directives(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedTripleSlashDirectives { .. }),
      "{err:?}",
    );

    let x = parse("///   <reference   no-default-lib = \"true\"/>");
    let err = super::check_for_banned_triple_slash_directives(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedTripleSlashDirectives { .. }),
      "{err:?}",
    );

    let x = parse("    /// <reference   lib = \"dom\"/>");
    let err = super::check_for_banned_triple_slash_directives(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedTripleSlashDirectives { .. }),
      "{err:?}",
    );

    let x = parse("   ///   <reference   lib = \'dom\'/>");
    let err = super::check_for_banned_triple_slash_directives(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedTripleSlashDirectives { .. }),
      "{err:?}",
    );

    let x = parse("   //  /   <reference   lib = \'dom\'/>");
    super::check_for_banned_triple_slash_directives(&x).unwrap();

    let x = parse("   ///   <reference   lib = \'dom\'/>  asdasd");
    super::check_for_banned_triple_slash_directives(&x).unwrap();

    let x = parse("   //some text here/   <reference   lib = \'dom\'/>");
    super::check_for_banned_triple_slash_directives(&x).unwrap();

    let x = parse("/** /   <reference   lib = \'dom\'/> */");
    super::check_for_banned_triple_slash_directives(&x).unwrap();
  }

  #[test]
  fn banned_syntax() {
    let x = parse("let x = 1;");
    assert!(super::check_for_banned_syntax(&x).is_ok());

    let x = parse("global {}");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::GlobalTypeAugmentation { .. }),
      "{err:?}",
    );

    let x = parse("let x = 1; global {}");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::GlobalTypeAugmentation { .. }),
      "{err:?}",
    );

    let x = parse("declare module foo { }");
    assert!(super::check_for_banned_syntax(&x).is_ok());

    let x = parse("declare module \"x\" { }");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::GlobalTypeAugmentation { .. }),
      "{err:?}",
    );

    let x = parse("import foo from \"foo\"");
    assert!(super::check_for_banned_syntax(&x).is_ok());

    let x = parse("export as namespace React;");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::GlobalTypeAugmentation { .. }),
      "{err:?}",
    );

    let x = parse("export = {}");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::GlobalTypeAugmentation { .. }),
      "{err:?}",
    );

    let x = parse("import express = require('foo');");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::CommonJs { .. }),
      "{err:?}",
    );

    let x = parse("import express = React.foo;");
    assert!(super::check_for_banned_syntax(&x).is_ok());

    let x = parse("import './data.json' assert { type: 'json' }");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedImportAssertion { .. }),
      "{err:?}",
    );

    let x = parse("export { a } from './data.json' assert { type: 'json' }");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedImportAssertion { .. }),
      "{err:?}",
    );

    let x = parse("export * from './data.json' assert { type: 'json' }");
    let err = super::check_for_banned_syntax(&x).unwrap_err();
    assert!(
      matches!(err, super::PublishError::BannedImportAssertion { .. }),
      "{err:?}",
    );

    let x = parse("export * from './data.json' with { type: 'json' }");
    assert!(super::check_for_banned_syntax(&x).is_ok(), "{err:?}",);
  }
}
