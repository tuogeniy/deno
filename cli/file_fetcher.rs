// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.

use crate::colors;
use crate::http_cache::HttpCache;
use crate::http_util;
use crate::http_util::create_http_client;
use crate::http_util::FetchOnceResult;
use crate::media_type::MediaType;
use crate::permissions::Permissions;
use crate::text_encoding;
use deno_core::error::custom_error;
use deno_core::error::generic_error;
use deno_core::error::uri_error;
use deno_core::error::AnyError;
use deno_core::futures;
use deno_core::futures::future::FutureExt;
use deno_core::url;
use deno_core::url::Url;
use deno_core::ModuleSpecifier;
use deno_fetch::reqwest;
use log::info;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::result::Result;
use std::str;
use std::sync::Arc;
use std::sync::Mutex;

/// Structure representing a text document.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TextDocument {
  bytes: Vec<u8>,
  charset: Cow<'static, str>,
}

impl TextDocument {
  pub fn new(
    bytes: Vec<u8>,
    charset: Option<impl Into<Cow<'static, str>>>,
  ) -> TextDocument {
    let charset = charset
      .map(|cs| cs.into())
      .unwrap_or_else(|| text_encoding::detect_charset(&bytes).into());
    TextDocument { bytes, charset }
  }

  pub fn as_bytes(&self) -> &Vec<u8> {
    &self.bytes
  }

  pub fn into_bytes(self) -> Vec<u8> {
    self.bytes
  }

  pub fn to_str(&self) -> Result<Cow<str>, std::io::Error> {
    text_encoding::convert_to_utf8(&self.bytes, &self.charset)
  }

  pub fn to_string(&self) -> Result<String, std::io::Error> {
    self.to_str().map(String::from)
  }
}

impl From<Vec<u8>> for TextDocument {
  fn from(bytes: Vec<u8>) -> Self {
    TextDocument::new(bytes, Option::<&str>::None)
  }
}

impl From<String> for TextDocument {
  fn from(s: String) -> Self {
    TextDocument::new(s.as_bytes().to_vec(), Option::<&str>::None)
  }
}

impl From<&str> for TextDocument {
  fn from(s: &str) -> Self {
    TextDocument::new(s.as_bytes().to_vec(), Option::<&str>::None)
  }
}

/// Structure representing local or remote file.
///
/// In case of remote file `url` might be different than originally requested URL, if so
/// `redirect_source_url` will contain original URL and `url` will be equal to final location.
#[derive(Debug, Clone)]
pub struct SourceFile {
  pub url: Url,
  pub filename: PathBuf,
  pub types_header: Option<String>,
  pub media_type: MediaType,
  pub source_code: TextDocument,
}

/// Simple struct implementing in-process caching to prevent multiple
/// fs reads/net fetches for same file.
#[derive(Clone, Default)]
pub struct SourceFileCache(Arc<Mutex<HashMap<String, SourceFile>>>);

impl SourceFileCache {
  pub fn set(&self, key: String, source_file: SourceFile) {
    let mut c = self.0.lock().unwrap();
    c.insert(key, source_file);
  }

  pub fn get(&self, key: String) -> Option<SourceFile> {
    let c = self.0.lock().unwrap();
    match c.get(&key) {
      Some(source_file) => Some(source_file.clone()),
      None => None,
    }
  }
}

const SUPPORTED_URL_SCHEMES: [&str; 3] = ["http", "https", "file"];

#[derive(Clone)]
pub struct SourceFileFetcher {
  source_file_cache: SourceFileCache,
  cache_blocklist: Vec<String>,
  use_disk_cache: bool,
  no_remote: bool,
  cached_only: bool,
  http_client: reqwest::Client,
  // This field is public only to expose it's location
  pub http_cache: HttpCache,
}

impl SourceFileFetcher {
  pub fn new(
    http_cache: HttpCache,
    use_disk_cache: bool,
    cache_blocklist: Vec<String>,
    no_remote: bool,
    cached_only: bool,
    ca_file: Option<&str>,
  ) -> Result<Self, AnyError> {
    let file_fetcher = Self {
      http_cache,
      source_file_cache: SourceFileCache::default(),
      cache_blocklist,
      use_disk_cache,
      no_remote,
      cached_only,
      http_client: create_http_client(ca_file)?,
    };

    Ok(file_fetcher)
  }

  pub fn check_if_supported_scheme(url: &Url) -> Result<(), AnyError> {
    if !SUPPORTED_URL_SCHEMES.contains(&url.scheme()) {
      return Err(generic_error(format!(
        "Unsupported scheme \"{}\" for module \"{}\". Supported schemes: {:#?}",
        url.scheme(),
        url,
        SUPPORTED_URL_SCHEMES
      )));
    }

    Ok(())
  }

  /// Required for TS compiler and source maps.
  pub fn fetch_cached_source_file(
    &self,
    specifier: &ModuleSpecifier,
    permissions: Permissions,
  ) -> Option<SourceFile> {
    let maybe_source_file = self.source_file_cache.get(specifier.to_string());

    if maybe_source_file.is_some() {
      return maybe_source_file;
    }

    // If file is not in memory cache check if it can be found
    // in local cache - which effectively means trying to fetch
    // using "--cached-only" flag.
    // It should be safe to for caller block on this
    // future, because it doesn't actually do any asynchronous
    // action in that path.
    if let Ok(maybe_source_file) =
      self.get_source_file_from_local_cache(specifier.as_url(), &permissions)
    {
      return maybe_source_file;
    }

    None
  }

  /// Save a given source file into cache.
  /// Allows injection of files that normally would not present
  /// in filesystem.
  /// This is useful when e.g. TS compiler retrieves a custom_error file
  /// under a dummy specifier.
  pub fn save_source_file_in_cache(
    &self,
    specifier: &ModuleSpecifier,
    file: SourceFile,
  ) {
    self.source_file_cache.set(specifier.to_string(), file);
  }

  pub async fn fetch_source_file(
    &self,
    specifier: &ModuleSpecifier,
    maybe_referrer: Option<ModuleSpecifier>,
    permissions: Permissions,
  ) -> Result<SourceFile, AnyError> {
    let module_url = specifier.as_url().to_owned();
    debug!(
      "fetch_source_file specifier: {} maybe_referrer: {:#?}",
      &module_url,
      maybe_referrer.as_ref()
    );

    // Check if this file was already fetched and can be retrieved from in-process cache.
    let maybe_cached_file = self.source_file_cache.get(specifier.to_string());
    if let Some(source_file) = maybe_cached_file {
      return Ok(source_file);
    }

    let source_file_cache = self.source_file_cache.clone();
    let specifier_ = specifier.clone();

    let result = self
      .get_source_file(
        &module_url,
        self.use_disk_cache,
        self.no_remote,
        self.cached_only,
        &permissions,
      )
      .await;

    match result {
      Ok(mut file) => {
        // TODO: move somewhere?
        if file.source_code.bytes.starts_with(b"#!") {
          file.source_code =
            filter_shebang(&file.source_code.to_str().unwrap()[..]).into();
        }

        // Cache in-process for subsequent access.
        source_file_cache.set(specifier_.to_string(), file.clone());

        Ok(file)
      }
      Err(err) => {
        // FIXME(bartlomieju): rewrite this whole block

        // FIXME(bartlomieju): very ugly
        let mut is_not_found = false;
        if let Some(e) = err.downcast_ref::<std::io::Error>() {
          if e.kind() == std::io::ErrorKind::NotFound {
            is_not_found = true;
          }
        }
        let referrer_suffix = if let Some(referrer) = maybe_referrer {
          format!(r#" from "{}""#, referrer)
        } else {
          "".to_owned()
        };
        // Hack: Check error message for "--cached-only" because the kind
        // conflicts with other errors.
        let err = if err.to_string().contains("--cached-only") {
          let msg = format!(
            r#"Cannot find module "{}"{} in cache, --cached-only is specified"#,
            module_url, referrer_suffix
          );
          custom_error("NotFound", msg)
        } else if is_not_found {
          let msg = format!(
            r#"Cannot resolve module "{}"{}"#,
            module_url, referrer_suffix
          );
          custom_error("NotFound", msg)
        } else {
          err
        };
        Err(err)
      }
    }
  }

  fn get_source_file_from_local_cache(
    &self,
    module_url: &Url,
    permissions: &Permissions,
  ) -> Result<Option<SourceFile>, AnyError> {
    let url_scheme = module_url.scheme();
    let is_local_file = url_scheme == "file";
    SourceFileFetcher::check_if_supported_scheme(&module_url)?;

    // Local files are always fetched from disk bypassing cache entirely.
    if is_local_file {
      return self.fetch_local_file(&module_url, permissions).map(Some);
    }

    self.fetch_cached_remote_source(&module_url, 10)
  }

  /// This is main method that is responsible for fetching local or remote files.
  ///
  /// If this is a remote module, and it has not yet been cached, the resulting
  /// download will be cached on disk for subsequent access.
  ///
  /// If `use_disk_cache` is true then remote files are fetched from disk cache.
  ///
  /// If `no_remote` is true then this method will fail for remote files.
  ///
  /// If `cached_only` is true then this method will fail for remote files
  /// not already cached.
  async fn get_source_file(
    &self,
    module_url: &Url,
    use_disk_cache: bool,
    no_remote: bool,
    cached_only: bool,
    permissions: &Permissions,
  ) -> Result<SourceFile, AnyError> {
    let url_scheme = module_url.scheme();
    let is_local_file = url_scheme == "file";
    SourceFileFetcher::check_if_supported_scheme(&module_url)?;

    // Local files are always fetched from disk bypassing cache entirely.
    if is_local_file {
      return self.fetch_local_file(&module_url, permissions);
    }

    // The file is remote, fail if `no_remote` is true.
    if no_remote {
      let e = std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!(
          "Not allowed to get remote file '{}'",
          module_url.to_string()
        ),
      );
      return Err(e.into());
    }

    // Fetch remote file and cache on-disk for subsequent access
    self
      .fetch_remote_source(
        &module_url,
        use_disk_cache,
        cached_only,
        10,
        permissions,
      )
      .await
  }

  /// Fetch local source file.
  fn fetch_local_file(
    &self,
    module_url: &Url,
    permissions: &Permissions,
  ) -> Result<SourceFile, AnyError> {
    let filepath = module_url
      .to_file_path()
      .map_err(|()| uri_error("File URL contains invalid path"))?;

    permissions.check_read(&filepath)?;
    let source_code = match fs::read(filepath.clone()) {
      Ok(c) => c,
      Err(e) => return Err(e.into()),
    };

    let (media_type, charset) = map_content_type(&filepath, None);
    Ok(SourceFile {
      url: module_url.clone(),
      filename: filepath,
      media_type,
      source_code: TextDocument::new(source_code, charset),
      types_header: None,
    })
  }

  /// Fetch cached remote file.
  ///
  /// This is a recursive operation if source file has redirections.
  ///
  /// It will keep reading <filename>.metadata.json for information about redirection.
  /// `module_initial_source_name` would be None on first call,
  /// and becomes the name of the very first module that initiates the call
  /// in subsequent recursions.
  ///
  /// AKA if redirection occurs, module_initial_source_name is the source path
  /// that user provides, and the final module_name is the resolved path
  /// after following all redirections.
  fn fetch_cached_remote_source(
    &self,
    module_url: &Url,
    redirect_limit: i64,
  ) -> Result<Option<SourceFile>, AnyError> {
    if redirect_limit < 0 {
      return Err(custom_error("Http", "too many redirects"));
    }

    let result = self.http_cache.get(&module_url);
    let result = match result {
      Err(e) => {
        if let Some(e) = e.downcast_ref::<std::io::Error>() {
          if e.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
          }
        }
        return Err(e);
      }
      Ok(c) => c,
    };

    let (mut source_file, headers) = result;
    if let Some(redirect_to) = headers.get("location") {
      let redirect_url = match Url::parse(redirect_to) {
        Ok(redirect_url) => redirect_url,
        Err(url::ParseError::RelativeUrlWithoutBase) => {
          let mut url = module_url.clone();
          url.set_path(redirect_to);
          url
        }
        Err(e) => {
          return Err(e.into());
        }
      };
      return self
        .fetch_cached_remote_source(&redirect_url, redirect_limit - 1);
    }

    let mut source_code = Vec::new();
    source_file.read_to_end(&mut source_code)?;

    let cache_filename = self.http_cache.get_cache_filename(module_url);
    let fake_filepath = PathBuf::from(module_url.path());
    let (media_type, charset) = map_content_type(
      &fake_filepath,
      headers.get("content-type").map(|e| e.as_str()),
    );
    let types_header = headers.get("x-typescript-types").map(|e| e.to_string());
    Ok(Some(SourceFile {
      url: module_url.clone(),
      filename: cache_filename,
      media_type,
      source_code: TextDocument::new(source_code, charset),
      types_header,
    }))
  }

  /// Asynchronously fetch remote source file specified by the URL following redirects.
  ///
  /// Note that this is a recursive method so it can't be "async", but rather return
  /// Pin<Box<..>>.
  fn fetch_remote_source(
    &self,
    module_url: &Url,
    use_disk_cache: bool,
    cached_only: bool,
    redirect_limit: i64,
    permissions: &Permissions,
  ) -> Pin<Box<dyn Future<Output = Result<SourceFile, AnyError>>>> {
    if redirect_limit < 0 {
      let e = custom_error("Http", "too many redirects");
      return futures::future::err(e).boxed_local();
    }

    if let Err(e) = permissions.check_net_url(&module_url) {
      return futures::future::err(e).boxed_local();
    }

    let is_blocked =
      check_cache_blocklist(module_url, self.cache_blocklist.as_ref());
    // First try local cache
    if use_disk_cache && !is_blocked {
      match self.fetch_cached_remote_source(&module_url, redirect_limit) {
        Ok(Some(source_file)) => {
          return futures::future::ok(source_file).boxed_local();
        }
        Ok(None) => {
          // there's no cached version
        }
        Err(err) => {
          return futures::future::err(err).boxed_local();
        }
      }
    }

    // If file wasn't found in cache check if we can fetch it
    if cached_only {
      // We can't fetch remote file - bail out
      let message = format!(
        "Cannot find remote file '{}' in cache, --cached-only is specified",
        module_url
      );
      return futures::future::err(custom_error("NotFound", message))
        .boxed_local();
    }

    info!("{} {}", colors::green("Download"), module_url.to_string());

    let dir = self.clone();
    let module_url = module_url.clone();
    let module_etag = match self.http_cache.get(&module_url) {
      Ok((_, headers)) => headers.get("etag").map(String::from),
      Err(_) => None,
    };
    let permissions = permissions.clone();
    let http_client = self.http_client.clone();
    // Single pass fetch, either yields code or yields redirect.
    let f = async move {
      match http_util::fetch_once(http_client, &module_url, module_etag).await?
      {
        FetchOnceResult::NotModified => {
          let source_file =
            dir.fetch_cached_remote_source(&module_url, 10)?.unwrap();

          Ok(source_file)
        }
        FetchOnceResult::Redirect(new_module_url, headers) => {
          // If redirects, update module_name and filename for next looped call.
          dir.http_cache.set(&module_url, headers, &[])?;

          // Recurse
          dir
            .fetch_remote_source(
              &new_module_url,
              use_disk_cache,
              cached_only,
              redirect_limit - 1,
              &permissions,
            )
            .await
        }
        FetchOnceResult::Code(source, headers) => {
          // We land on the code.
          dir.http_cache.set(&module_url, headers.clone(), &source)?;

          let cache_filepath = dir.http_cache.get_cache_filename(&module_url);
          // Used to sniff out content type from file extension - probably to be removed
          let fake_filepath = PathBuf::from(module_url.path());
          let (media_type, charset) = map_content_type(
            &fake_filepath,
            headers.get("content-type").map(String::as_str),
          );

          let types_header =
            headers.get("x-typescript-types").map(String::to_string);

          let source_file = SourceFile {
            url: module_url.clone(),
            filename: cache_filepath,
            media_type,
            source_code: TextDocument::new(source, charset),
            types_header,
          };

          Ok(source_file)
        }
      }
    };

    f.boxed_local()
  }
}

// convert a ContentType string into a enumerated MediaType + optional charset
fn map_content_type(
  path: &Path,
  content_type: Option<&str>,
) -> (MediaType, Option<String>) {
  match content_type {
    Some(content_type) => {
      // Sometimes there is additional data after the media type in
      // Content-Type so we have to do a bit of manipulation so we are only
      // dealing with the actual media type.
      let mut ct_iter = content_type.split(';');
      let ct = ct_iter.next().unwrap();
      let media_type = match ct.to_lowercase().as_ref() {
        "application/typescript"
        | "text/typescript"
        | "video/vnd.dlna.mpeg-tts"
        | "video/mp2t"
        | "application/x-typescript" => {
          map_js_like_extension(path, MediaType::TypeScript)
        }
        "application/javascript"
        | "text/javascript"
        | "application/ecmascript"
        | "text/ecmascript"
        | "application/x-javascript"
        | "application/node" => {
          map_js_like_extension(path, MediaType::JavaScript)
        }
        "application/json" | "text/json" => MediaType::Json,
        "application/wasm" => MediaType::Wasm,
        // Handle plain and possibly webassembly
        "text/plain" | "application/octet-stream" => MediaType::from(path),
        _ => {
          debug!("unknown content type: {}", content_type);
          MediaType::Unknown
        }
      };

      let charset = ct_iter
        .map(str::trim)
        .find_map(|s| s.strip_prefix("charset="))
        .map(String::from);

      (media_type, charset)
    }
    None => (MediaType::from(path), None),
  }
}

fn map_js_like_extension(path: &Path, default: MediaType) -> MediaType {
  match path.extension() {
    None => default,
    Some(os_str) => match os_str.to_str() {
      None => default,
      Some("jsx") => MediaType::JSX,
      Some("tsx") => MediaType::TSX,
      Some(_) => default,
    },
  }
}

fn filter_shebang(string: &str) -> Vec<u8> {
  if let Some(i) = string.find('\n') {
    let (_, rest) = string.split_at(i);
    rest.as_bytes().to_owned()
  } else {
    Vec::new()
  }
}

fn check_cache_blocklist(url: &Url, black_list: &[String]) -> bool {
  let mut url_without_fragmets = url.clone();
  url_without_fragmets.set_fragment(None);
  if black_list.contains(&String::from(url_without_fragmets.as_str())) {
    return true;
  }
  let mut url_without_query_strings = url_without_fragmets;
  url_without_query_strings.set_query(None);
  let mut path_buf = PathBuf::from(url_without_query_strings.as_str());
  loop {
    if black_list.contains(&String::from(path_buf.to_str().unwrap())) {
      return true;
    }
    if !path_buf.pop() {
      break;
    }
  }
  false
}

#[derive(Debug, Default)]
/// Header metadata associated with a particular "symbolic" source code file.
/// (the associated source code file might not be cached, while remaining
/// a user accessible entity through imports (due to redirects)).
pub struct SourceCodeHeaders {
  /// MIME type of the source code.
  pub mime_type: Option<String>,
  /// Where should we actually look for source code.
  /// This should be an absolute path!
  pub redirect_to: Option<String>,
  /// ETag of the remote source file
  pub etag: Option<String>,
  /// X-TypeScript-Types defines the location of a .d.ts file
  pub x_typescript_types: Option<String>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  fn setup_file_fetcher(dir_path: &Path) -> SourceFileFetcher {
    SourceFileFetcher::new(
      HttpCache::new(&dir_path.to_path_buf().join("deps")),
      true,
      vec![],
      false,
      false,
      None,
    )
    .expect("setup fail")
  }

  fn test_setup() -> (TempDir, SourceFileFetcher) {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let fetcher = setup_file_fetcher(temp_dir.path());
    (temp_dir, fetcher)
  }

  macro_rules! file_url {
    ($path:expr) => {
      if cfg!(target_os = "windows") {
        concat!("file:///C:", $path)
      } else {
        concat!("file://", $path)
      }
    };
  }

  #[test]
  fn test_cache_blocklist() {
    let args = crate::flags::resolve_urls(vec![
      String::from("http://deno.land/std"),
      String::from("http://github.com/example/mod.ts"),
      String::from("http://fragment.com/mod.ts#fragment"),
      String::from("http://query.com/mod.ts?foo=bar"),
      String::from("http://queryandfragment.com/mod.ts?foo=bar#fragment"),
    ]);

    let u: Url = "http://deno.land/std/fs/mod.ts".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://github.com/example/file.ts".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), false);

    let u: Url = "http://github.com/example/mod.ts".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://github.com/example/mod.ts?foo=bar".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://github.com/example/mod.ts#fragment".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://fragment.com/mod.ts".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://query.com/mod.ts".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), false);

    let u: Url = "http://fragment.com/mod.ts#fragment".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://query.com/mod.ts?foo=bar".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://queryandfragment.com/mod.ts".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), false);

    let u: Url = "http://queryandfragment.com/mod.ts?foo=bar"
      .parse()
      .unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://queryandfragment.com/mod.ts#fragment"
      .parse()
      .unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), false);

    let u: Url = "http://query.com/mod.ts?foo=bar#fragment".parse().unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);

    let u: Url = "http://fragment.com/mod.ts?foo=bar#fragment"
      .parse()
      .unwrap();
    assert_eq!(check_cache_blocklist(&u, &args), true);
  }

  #[test]
  fn test_fetch_local_file_no_panic() {
    let (_temp_dir, fetcher) = test_setup();
    if cfg!(windows) {
      // Should fail: missing drive letter.
      let u = Url::parse("file:///etc/passwd").unwrap();
      fetcher
        .fetch_local_file(&u, &Permissions::allow_all())
        .unwrap_err();
    } else {
      // Should fail: local network paths are not supported on unix.
      let u = Url::parse("file://server/etc/passwd").unwrap();
      fetcher
        .fetch_local_file(&u, &Permissions::allow_all())
        .unwrap_err();
    }
  }

  #[tokio::test]
  async fn test_get_source_code_1() {
    let _http_server_guard = test_util::http_server();
    let (temp_dir, fetcher) = test_setup();
    let fetcher_1 = fetcher.clone();
    let fetcher_2 = fetcher.clone();
    let module_url =
      Url::parse("http://localhost:4545/cli/tests/subdir/mod2.ts").unwrap();
    let module_url_1 = module_url.clone();
    let module_url_2 = module_url.clone();

    let cache_filename = fetcher.http_cache.get_cache_filename(&module_url);

    let result = fetcher
      .get_source_file(
        &module_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let r = result.unwrap();
    assert_eq!(
      r.source_code.bytes,
      &b"export { printHello } from \"./print_hello.ts\";\n"[..]
    );
    assert_eq!(&(r.media_type), &MediaType::TypeScript);

    let mut metadata =
      crate::http_cache::Metadata::read(&cache_filename).unwrap();

    // Modify .headers.json, write using fs write
    metadata.headers = HashMap::new();
    metadata
      .headers
      .insert("content-type".to_string(), "text/javascript".to_string());
    metadata.write(&cache_filename).unwrap();

    let result2 = fetcher_1
      .get_source_file(
        &module_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result2.is_ok());
    let r2 = result2.unwrap();
    assert_eq!(
      r2.source_code.bytes,
      &b"export { printHello } from \"./print_hello.ts\";\n"[..]
    );
    // If get_source_file does not call remote, this should be JavaScript
    // as we modified before! (we do not overwrite .headers.json due to no http fetch)
    assert_eq!(&(r2.media_type), &MediaType::JavaScript);
    let (_, headers) = fetcher_2.http_cache.get(&module_url_1).unwrap();

    assert_eq!(headers.get("content-type").unwrap(), "text/javascript");

    // Modify .headers.json again, but the other way around
    metadata.headers = HashMap::new();
    metadata
      .headers
      .insert("content-type".to_string(), "application/json".to_string());
    metadata.write(&cache_filename).unwrap();

    let result3 = fetcher_2
      .get_source_file(
        &module_url_1,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result3.is_ok());
    let r3 = result3.unwrap();
    assert_eq!(
      r3.source_code.bytes,
      &b"export { printHello } from \"./print_hello.ts\";\n"[..]
    );
    // If get_source_file does not call remote, this should be JavaScript
    // as we modified before! (we do not overwrite .headers.json due to no http fetch)
    assert_eq!(&(r3.media_type), &MediaType::Json);
    let metadata = crate::http_cache::Metadata::read(&cache_filename).unwrap();
    assert_eq!(
      metadata.headers.get("content-type").unwrap(),
      "application/json"
    );

    // let's create fresh instance of DenoDir (simulating another freshh Deno process)
    // and don't use cache
    let fetcher = setup_file_fetcher(temp_dir.path());
    let result4 = fetcher
      .get_source_file(
        &module_url_2,
        false,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result4.is_ok());
    let r4 = result4.unwrap();
    let expected4 = &b"export { printHello } from \"./print_hello.ts\";\n"[..];
    assert_eq!(r4.source_code.bytes, expected4);
    // Resolved back to TypeScript
    assert_eq!(&(r4.media_type), &MediaType::TypeScript);
  }

  #[tokio::test]
  async fn test_get_source_code_2() {
    let _http_server_guard = test_util::http_server();
    let (temp_dir, fetcher) = test_setup();
    let module_url =
      Url::parse("http://localhost:4545/cli/tests/subdir/mismatch_ext.ts")
        .unwrap();
    let module_url_1 = module_url.clone();

    let cache_filename = fetcher.http_cache.get_cache_filename(&module_url);

    let result = fetcher
      .get_source_file(
        &module_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let r = result.unwrap();
    let expected = b"export const loaded = true;\n";
    assert_eq!(r.source_code.bytes, expected);
    assert_eq!(&(r.media_type), &MediaType::JavaScript);
    let (_, headers) = fetcher.http_cache.get(&module_url).unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/javascript");

    // Modify .headers.json
    let mut metadata =
      crate::http_cache::Metadata::read(&cache_filename).unwrap();
    metadata.headers = HashMap::new();
    metadata
      .headers
      .insert("content-type".to_string(), "text/typescript".to_string());
    metadata.write(&cache_filename).unwrap();

    let result2 = fetcher
      .get_source_file(
        &module_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result2.is_ok());
    let r2 = result2.unwrap();
    let expected2 = b"export const loaded = true;\n";
    assert_eq!(r2.source_code.bytes, expected2);
    // If get_source_file does not call remote, this should be TypeScript
    // as we modified before! (we do not overwrite .headers.json due to no http
    // fetch)
    assert_eq!(&(r2.media_type), &MediaType::TypeScript);
    let metadata = crate::http_cache::Metadata::read(&cache_filename).unwrap();
    assert_eq!(
      metadata.headers.get("content-type").unwrap(),
      "text/typescript"
    );

    // let's create fresh instance of DenoDir (simulating another fresh Deno
    // process) and don't use cache
    let fetcher = setup_file_fetcher(temp_dir.path());
    let result3 = fetcher
      .get_source_file(
        &module_url_1,
        false,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result3.is_ok());
    let r3 = result3.unwrap();
    let expected3 = b"export const loaded = true;\n";
    assert_eq!(r3.source_code.bytes, expected3);
    // Now the old .headers.json file should be overwritten back to JavaScript!
    // (due to http fetch)
    assert_eq!(&(r3.media_type), &MediaType::JavaScript);
    let (_, headers) = fetcher.http_cache.get(&module_url).unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/javascript");
  }

  #[tokio::test]
  async fn test_get_source_code_multiple_downloads_of_same_file() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let specifier = ModuleSpecifier::resolve_url(
      "http://localhost:4545/cli/tests/subdir/mismatch_ext.ts",
    )
    .unwrap();
    let cache_filename =
      fetcher.http_cache.get_cache_filename(&specifier.as_url());

    // first download
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_ok());

    let headers_file_name =
      crate::http_cache::Metadata::filename(&cache_filename);
    let result = fs::File::open(&headers_file_name);
    assert!(result.is_ok());
    let headers_file = result.unwrap();
    // save modified timestamp for headers file
    let headers_file_metadata = headers_file.metadata().unwrap();
    let headers_file_modified = headers_file_metadata.modified().unwrap();

    // download file again, it should use already fetched file even though
    // `use_disk_cache` is set to false, this can be verified using source
    // header file creation timestamp (should be the same as after first
    // download)
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_ok());

    let result = fs::File::open(&headers_file_name);
    assert!(result.is_ok());
    let headers_file_2 = result.unwrap();
    // save modified timestamp for headers file
    let headers_file_metadata_2 = headers_file_2.metadata().unwrap();
    let headers_file_modified_2 = headers_file_metadata_2.modified().unwrap();

    assert_eq!(headers_file_modified, headers_file_modified_2);
  }

  #[tokio::test]
  async fn test_get_source_code_3() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();

    let redirect_module_url = Url::parse(
      "http://localhost:4546/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirect_source_filepath =
      fetcher.http_cache.get_cache_filename(&redirect_module_url);
    let redirect_source_filename =
      redirect_source_filepath.to_str().unwrap().to_string();
    let target_module_url = Url::parse(
      "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirect_target_filepath =
      fetcher.http_cache.get_cache_filename(&target_module_url);
    let redirect_target_filename =
      redirect_target_filepath.to_str().unwrap().to_string();

    // Test basic follow and headers recording
    let result = fetcher
      .get_source_file(
        &redirect_module_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let mod_meta = result.unwrap();
    // File that requires redirection should be empty file.
    assert_eq!(fs::read_to_string(&redirect_source_filename).unwrap(), "");
    let (_, headers) = fetcher.http_cache.get(&redirect_module_url).unwrap();
    assert_eq!(
      headers.get("location").unwrap(),
      "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js"
    );
    // The target of redirection is downloaded instead.
    assert_eq!(
      fs::read_to_string(&redirect_target_filename).unwrap(),
      "export const redirect = 1;\n"
    );
    let (_, headers) = fetcher.http_cache.get(&target_module_url).unwrap();
    assert!(headers.get("location").is_none());
    // Examine the meta result.
    assert_eq!(mod_meta.url, target_module_url);
  }

  #[tokio::test]
  async fn test_get_source_code_4() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let double_redirect_url = Url::parse(
      "http://localhost:4548/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let double_redirect_path =
      fetcher.http_cache.get_cache_filename(&double_redirect_url);

    let redirect_url = Url::parse(
      "http://localhost:4546/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirect_path = fetcher.http_cache.get_cache_filename(&redirect_url);

    let target_url = Url::parse(
      "http://localhost:4545/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let target_path = fetcher.http_cache.get_cache_filename(&target_url);

    // Test double redirects and headers recording
    let result = fetcher
      .get_source_file(
        &double_redirect_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let mod_meta = result.unwrap();
    assert_eq!(fs::read_to_string(&double_redirect_path).unwrap(), "");
    assert_eq!(fs::read_to_string(&redirect_path).unwrap(), "");

    let (_, headers) = fetcher.http_cache.get(&double_redirect_url).unwrap();
    assert_eq!(headers.get("location").unwrap(), &redirect_url.to_string());

    let (_, headers) = fetcher.http_cache.get(&redirect_url).unwrap();
    assert_eq!(headers.get("location").unwrap(), &target_url.to_string());

    // The target of redirection is downloaded instead.
    assert_eq!(
      fs::read_to_string(&target_path).unwrap(),
      "export const redirect = 1;\n"
    );
    let (_, headers) = fetcher.http_cache.get(&target_url).unwrap();
    assert!(headers.get("location").is_none());

    // Examine the meta result.
    assert_eq!(mod_meta.url, target_url);
  }

  #[tokio::test]
  async fn test_get_source_code_5() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();

    let double_redirect_url = Url::parse(
      "http://localhost:4548/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();

    let redirect_url = Url::parse(
      "http://localhost:4546/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();

    let target_path = fetcher.http_cache.get_cache_filename(&redirect_url);
    let target_path_ = target_path.clone();

    // Test that redirect target is not downloaded twice for different redirect source.
    let result = fetcher
      .get_source_file(
        &double_redirect_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let result = fs::File::open(&target_path);
    assert!(result.is_ok());
    let file = result.unwrap();
    // save modified timestamp for headers file of redirect target
    let file_metadata = file.metadata().unwrap();
    let file_modified = file_metadata.modified().unwrap();

    // When another file is fetched that also point to redirect target, then
    // redirect target shouldn't be downloaded again. It can be verified
    // using source header file creation timestamp (should be the same as
    // after first `get_source_file`)
    let result = fetcher
      .get_source_file(
        &redirect_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let result = fs::File::open(&target_path_);
    assert!(result.is_ok());
    let file_2 = result.unwrap();
    // save modified timestamp for headers file
    let file_metadata_2 = file_2.metadata().unwrap();
    let file_modified_2 = file_metadata_2.modified().unwrap();

    assert_eq!(file_modified, file_modified_2);
  }

  #[tokio::test]
  async fn test_get_source_code_6() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let double_redirect_url = Url::parse(
      "http://localhost:4548/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();

    // Test that redirections can be limited
    let result = fetcher
      .fetch_remote_source(
        &double_redirect_url,
        false,
        false,
        2,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());

    let result = fetcher
      .fetch_remote_source(
        &double_redirect_url,
        false,
        false,
        1,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_err());

    // Test that redirections in cached files are limited as well
    let result = fetcher.fetch_cached_remote_source(&double_redirect_url, 2);
    assert!(result.is_ok());

    let result = fetcher.fetch_cached_remote_source(&double_redirect_url, 1);
    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_get_source_code_7() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();

    // Testing redirect with Location set to absolute url.
    let redirect_module_url = Url::parse(
      "http://localhost:4550/REDIRECT/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirect_source_filepath =
      fetcher.http_cache.get_cache_filename(&redirect_module_url);
    let redirect_source_filename =
      redirect_source_filepath.to_str().unwrap().to_string();
    let target_module_url = Url::parse(
      "http://localhost:4550/cli/tests/subdir/redirects/redirect1.js",
    )
    .unwrap();
    let redirect_target_filepath =
      fetcher.http_cache.get_cache_filename(&target_module_url);
    let redirect_target_filename =
      redirect_target_filepath.to_str().unwrap().to_string();

    // Test basic follow and headers recording
    let result = fetcher
      .get_source_file(
        &redirect_module_url,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let mod_meta = result.unwrap();
    // File that requires redirection should be empty file.
    assert_eq!(fs::read_to_string(&redirect_source_filename).unwrap(), "");
    let (_, headers) = fetcher.http_cache.get(&redirect_module_url).unwrap();
    assert_eq!(
      headers.get("location").unwrap(),
      "/cli/tests/subdir/redirects/redirect1.js"
    );
    // The target of redirection is downloaded instead.
    assert_eq!(
      fs::read_to_string(&redirect_target_filename).unwrap(),
      "export const redirect = 1;\n"
    );
    let (_, headers) = fetcher.http_cache.get(&target_module_url).unwrap();
    assert!(headers.get("location").is_none());
    // Examine the meta result.
    assert_eq!(mod_meta.url, target_module_url);
  }

  #[tokio::test]
  async fn test_get_source_no_remote() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url =
      Url::parse("http://localhost:4545/cli/tests/002_hello.ts").unwrap();
    // Remote modules are not allowed
    let result = fetcher
      .get_source_file(
        &module_url,
        true,
        true,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_err());
    // FIXME(bartlomieju):
    // let err = result.err().unwrap();
    // assert_eq!(err.kind(), ErrorKind::NotFound);
  }

  #[tokio::test]
  async fn test_get_source_cached_only() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let fetcher_1 = fetcher.clone();
    let fetcher_2 = fetcher.clone();
    let module_url =
      Url::parse("http://localhost:4545/cli/tests/002_hello.ts").unwrap();
    let module_url_1 = module_url.clone();
    let module_url_2 = module_url.clone();

    // file hasn't been cached before
    let result = fetcher
      .get_source_file(
        &module_url,
        true,
        false,
        true,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_err());
    // FIXME(bartlomieju):
    // let err = result.err().unwrap();
    // assert_eq!(err.kind(), ErrorKind::NotFound);

    // download and cache file
    let result = fetcher_1
      .get_source_file(
        &module_url_1,
        true,
        false,
        false,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    // module is already cached, should be ok even with `cached_only`
    let result = fetcher_2
      .get_source_file(
        &module_url_2,
        true,
        false,
        true,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_fetch_source_0() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url =
      Url::parse("http://127.0.0.1:4545/cli/tests/subdir/mt_video_mp2t.t3.ts")
        .unwrap();
    let result = fetcher
      .fetch_remote_source(
        &module_url,
        false,
        false,
        10,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let r = result.unwrap();
    assert_eq!(r.source_code.bytes, b"export const loaded = true;\n");
    assert_eq!(&(r.media_type), &MediaType::TypeScript);

    // Modify .metadata.json, make sure read from local
    let cache_filename = fetcher.http_cache.get_cache_filename(&module_url);
    let mut metadata =
      crate::http_cache::Metadata::read(&cache_filename).unwrap();
    metadata.headers = HashMap::new();
    metadata
      .headers
      .insert("content-type".to_string(), "text/javascript".to_string());
    metadata.write(&cache_filename).unwrap();

    let result2 = fetcher.fetch_cached_remote_source(&module_url, 1);
    assert!(result2.is_ok());
    let r2 = result2.unwrap().unwrap();
    assert_eq!(r2.source_code.bytes, b"export const loaded = true;\n");
    // Not MediaType::TypeScript due to .headers.json modification
    assert_eq!(&(r2.media_type), &MediaType::JavaScript);
  }

  #[tokio::test]
  async fn fetch_remote_source_no_ext() {
    let _g = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url =
      &Url::parse("http://localhost:4545/cli/tests/subdir/no_ext").unwrap();
    let result = fetcher
      .fetch_remote_source(
        module_url,
        false,
        false,
        10,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let r = result.unwrap();
    assert_eq!(r.source_code.bytes, b"export const loaded = true;\n");
    assert_eq!(&(r.media_type), &MediaType::TypeScript);
    let (_, headers) = fetcher.http_cache.get(module_url).unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/typescript");
  }

  #[tokio::test]
  async fn fetch_remote_source_mismatch_ext() {
    let _g = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url =
      &Url::parse("http://localhost:4545/cli/tests/subdir/mismatch_ext.ts")
        .unwrap();
    let result = fetcher
      .fetch_remote_source(
        module_url,
        false,
        false,
        10,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let r2 = result.unwrap();
    assert_eq!(r2.source_code.bytes, b"export const loaded = true;\n");
    assert_eq!(&(r2.media_type), &MediaType::JavaScript);
    let (_, headers) = fetcher.http_cache.get(module_url).unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/javascript");
  }

  #[tokio::test]
  async fn fetch_remote_source_unknown_ext() {
    let _g = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url =
      &Url::parse("http://localhost:4545/cli/tests/subdir/unknown_ext.deno")
        .unwrap();
    let result = fetcher
      .fetch_remote_source(
        module_url,
        false,
        false,
        10,
        &Permissions::allow_all(),
      )
      .await;
    assert!(result.is_ok());
    let r3 = result.unwrap();
    assert_eq!(r3.source_code.bytes, b"export const loaded = true;\n");
    assert_eq!(&(r3.media_type), &MediaType::TypeScript);
    let (_, headers) = fetcher.http_cache.get(module_url).unwrap();
    assert_eq!(headers.get("content-type").unwrap(), "text/typescript");
  }

  #[tokio::test]
  async fn test_fetch_source_file() {
    let (_temp_dir, fetcher) = test_setup();

    // Test failure case.
    let specifier =
      ModuleSpecifier::resolve_url(file_url!("/baddir/hello.ts")).unwrap();
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_err());

    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("rt/99_main.js");
    let specifier =
      ModuleSpecifier::resolve_url_or_path(p.to_str().unwrap()).unwrap();
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_ok());
  }

  #[tokio::test]
  async fn test_fetch_source_file_1() {
    /*recompile ts file*/
    let (_temp_dir, fetcher) = test_setup();

    // Test failure case.
    let specifier =
      ModuleSpecifier::resolve_url(file_url!("/baddir/hello.ts")).unwrap();
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_err());

    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("rt/99_main.js");
    let specifier =
      ModuleSpecifier::resolve_url_or_path(p.to_str().unwrap()).unwrap();
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_ok());
  }

  #[tokio::test]
  async fn test_fetch_source_file_2() {
    /*recompile ts file*/
    let (_temp_dir, fetcher) = test_setup();

    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests/001_hello.js");
    let specifier =
      ModuleSpecifier::resolve_url_or_path(p.to_str().unwrap()).unwrap();
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_ok());
  }

  #[test]
  fn test_resolve_module_3() {
    // unsupported schemes
    let test_cases = [
      "ftp://localhost:4545/testdata/subdir/print_hello.ts",
      "blob:https://whatwg.org/d0360e2f-caee-469f-9a2f-87d5b0456f6f",
    ];

    for &test in test_cases.iter() {
      let url = Url::parse(test).unwrap();
      assert!(SourceFileFetcher::check_if_supported_scheme(&url).is_err());
    }
  }

  async fn test_fetch_source_file_from_disk_nonstandard_encoding(
    charset: &str,
    expected_content: String,
  ) {
    let (_temp_dir, fetcher) = test_setup();

    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join(format!("tests/encoding/{}.ts", charset));
    let specifier =
      ModuleSpecifier::resolve_url_or_path(p.to_str().unwrap()).unwrap();
    let r = fetcher
      .fetch_source_file(&specifier, None, Permissions::allow_all())
      .await;
    assert!(r.is_ok());
    let fetched_file = r.unwrap();
    let source_code = fetched_file.source_code.to_str();
    assert!(source_code.is_ok());
    let actual = source_code.unwrap();
    assert_eq!(expected_content, actual);
  }

  #[tokio::test]
  async fn test_fetch_source_file_from_disk_utf_16_be() {
    test_fetch_source_file_from_disk_nonstandard_encoding(
      "utf-16be",
      String::from_utf8(
        b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A".to_vec(),
      )
      .unwrap(),
    )
    .await;
  }

  #[tokio::test]
  async fn test_fetch_source_file_from_disk_utf_16_le() {
    test_fetch_source_file_from_disk_nonstandard_encoding(
      "utf-16le",
      String::from_utf8(
        b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A".to_vec(),
      )
      .unwrap(),
    )
    .await;
  }

  #[tokio::test]
  async fn test_fetch_source_file_from_disk_utf_8_with_bom() {
    test_fetch_source_file_from_disk_nonstandard_encoding(
      "utf-8",
      String::from_utf8(
        b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A".to_vec(),
      )
      .unwrap(),
    )
    .await;
  }

  #[test]
  fn test_map_content_type_extension_only() {
    // Extension only
    assert_eq!(
      map_content_type(Path::new("foo/bar.ts"), None).0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.tsx"), None).0,
      MediaType::TSX
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.d.ts"), None).0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.js"), None).0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.txt"), None).0,
      MediaType::Unknown
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.jsx"), None).0,
      MediaType::JSX
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.json"), None).0,
      MediaType::Json
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.wasm"), None).0,
      MediaType::Wasm
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.cjs"), None).0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), None).0,
      MediaType::Unknown
    );
  }

  #[test]
  fn test_map_content_type_media_type_with_no_extension() {
    // Media Type
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("application/typescript")).0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("text/typescript")).0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("video/vnd.dlna.mpeg-tts")).0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("video/mp2t")).0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("application/x-typescript"))
        .0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("application/javascript")).0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("text/javascript")).0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("application/ecmascript")).0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("text/ecmascript")).0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("application/x-javascript"))
        .0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("application/json")).0,
      MediaType::Json
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("application/node")).0,
      MediaType::JavaScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("text/json")).0,
      MediaType::Json
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar"), Some("text/json; charset=utf-8 ")),
      (MediaType::Json, Some("utf-8".to_owned()))
    );
  }

  #[test]
  fn test_map_file_extension_media_type_with_extension() {
    assert_eq!(
      map_content_type(Path::new("foo/bar.ts"), Some("text/plain")).0,
      MediaType::TypeScript
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.ts"), Some("foo/bar")).0,
      MediaType::Unknown
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.tsx"),
        Some("application/typescript"),
      )
      .0,
      MediaType::TSX
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.tsx"),
        Some("application/javascript"),
      )
      .0,
      MediaType::TSX
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.tsx"),
        Some("application/x-typescript"),
      )
      .0,
      MediaType::TSX
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.tsx"),
        Some("video/vnd.dlna.mpeg-tts"),
      )
      .0,
      MediaType::TSX
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.tsx"), Some("video/mp2t")).0,
      MediaType::TSX
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.jsx"),
        Some("application/javascript"),
      )
      .0,
      MediaType::JSX
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.jsx"),
        Some("application/x-typescript"),
      )
      .0,
      MediaType::JSX
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.jsx"),
        Some("application/ecmascript"),
      )
      .0,
      MediaType::JSX
    );
    assert_eq!(
      map_content_type(Path::new("foo/bar.jsx"), Some("text/ecmascript")).0,
      MediaType::JSX
    );
    assert_eq!(
      map_content_type(
        Path::new("foo/bar.jsx"),
        Some("application/x-javascript"),
      )
      .0,
      MediaType::JSX
    );
  }

  #[test]
  fn test_filter_shebang() {
    assert_eq!(filter_shebang("#!"), b"");
    assert_eq!(filter_shebang("#!\n\n"), b"\n\n");
    let code = "#!/usr/bin/env deno\nconsole.log('hello');\n";
    assert_eq!(filter_shebang(code), b"\nconsole.log('hello');\n");
  }

  #[tokio::test]
  async fn test_fetch_with_etag() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url =
      Url::parse("http://127.0.0.1:4545/etag_script.ts").unwrap();

    let source = fetcher
      .fetch_remote_source(
        &module_url,
        false,
        false,
        1,
        &Permissions::allow_all(),
      )
      .await;
    assert!(source.is_ok());
    let source = source.unwrap();
    assert_eq!(source.source_code.bytes, b"console.log('etag')");
    assert_eq!(&(source.media_type), &MediaType::TypeScript);

    let (_, headers) = fetcher.http_cache.get(&module_url).unwrap();
    assert_eq!(headers.get("etag").unwrap(), "33a64df551425fcc55e");

    let metadata_path = crate::http_cache::Metadata::filename(
      &fetcher.http_cache.get_cache_filename(&module_url),
    );

    let modified1 = metadata_path.metadata().unwrap().modified().unwrap();

    // Forcibly change the contents of the cache file and request
    // it again with the cache parameters turned off.
    // If the fetched content changes, the cached content is used.
    let file_name = fetcher.http_cache.get_cache_filename(&module_url);
    let _ = fs::write(&file_name, "changed content");
    let cached_source = fetcher
      .fetch_remote_source(
        &module_url,
        false,
        false,
        1,
        &Permissions::allow_all(),
      )
      .await
      .unwrap();
    assert_eq!(cached_source.source_code.bytes, b"changed content");

    let modified2 = metadata_path.metadata().unwrap().modified().unwrap();

    // Assert that the file has not been modified
    assert_eq!(modified1, modified2);
  }

  #[tokio::test]
  async fn test_fetch_with_types_header() {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url =
      Url::parse("http://127.0.0.1:4545/xTypeScriptTypes.js").unwrap();
    let source = fetcher
      .fetch_remote_source(
        &module_url,
        false,
        false,
        1,
        &Permissions::allow_all(),
      )
      .await;
    assert!(source.is_ok());
    let source = source.unwrap();
    assert_eq!(source.source_code.bytes, b"export const foo = 'foo';");
    assert_eq!(&(source.media_type), &MediaType::JavaScript);
    assert_eq!(
      source.types_header,
      Some("./xTypeScriptTypes.d.ts".to_string())
    );
  }

  #[tokio::test]
  async fn test_fetch_source_file_from_net_utf16_le() {
    let content =
      std::str::from_utf8(b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A")
        .unwrap();
    test_fetch_non_utf8_source_file_from_net(
      "utf-16le",
      "utf-16le.ts",
      content,
    )
    .await;
  }

  #[tokio::test]
  async fn test_fetch_source_file_from_net_utf16_be() {
    let content =
      std::str::from_utf8(b"\xEF\xBB\xBFconsole.log(\"Hello World\");\x0A")
        .unwrap();
    test_fetch_non_utf8_source_file_from_net(
      "utf-16be",
      "utf-16be.ts",
      content,
    )
    .await;
  }

  #[tokio::test]
  async fn test_fetch_source_file_from_net_windows_1255() {
    let content = "console.log(\"\u{5E9}\u{5DC}\u{5D5}\u{5DD} \
                   \u{5E2}\u{5D5}\u{5DC}\u{5DD}\");\u{A}";
    test_fetch_non_utf8_source_file_from_net(
      "windows-1255",
      "windows-1255",
      content,
    )
    .await;
  }

  async fn test_fetch_non_utf8_source_file_from_net(
    charset: &str,
    file_name: &str,
    expected_content: &str,
  ) {
    let _http_server_guard = test_util::http_server();
    let (_temp_dir, fetcher) = test_setup();
    let module_url = Url::parse(&format!(
      "http://127.0.0.1:4545/cli/tests/encoding/{}",
      file_name
    ))
    .unwrap();

    let source = fetcher
      .fetch_remote_source(
        &module_url,
        false,
        false,
        1,
        &Permissions::allow_all(),
      )
      .await;
    assert!(source.is_ok());
    let source = source.unwrap();
    assert_eq!(&source.source_code.charset.to_lowercase()[..], charset);
    let text = &source.source_code.to_str().unwrap();
    assert_eq!(text, expected_content);
    assert_eq!(&(source.media_type), &MediaType::TypeScript);

    let (_, headers) = fetcher.http_cache.get(&module_url).unwrap();
    assert_eq!(
      headers.get("content-type").unwrap(),
      &format!("application/typescript;charset={}", charset)
    );
  }
}
