use crate::any::collect_any_types;
use crate::client::AutoReflectionClient;
use crate::editions;
use crate::error::Error::GenericError;
use crate::error::Result;
use crate::manager::GrpcConfig;
use anyhow::anyhow;
use async_recursion::async_recursion;
use log::{debug, info, warn};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor, ReflectMessage, Value};
use prost_types::FileDescriptorProto;
use std::collections::{BTreeMap, HashSet};
use std::env::temp_dir;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::RwLock;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::Uri;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
use yaak_common::command::new_xplatform_command;
use yaak_tls::ClientCertificateConfig;

pub async fn fill_pool_from_files(
    config: &GrpcConfig,
    paths: &Vec<PathBuf>,
) -> Result<DescriptorPool> {
    if let Some(buf_root) = find_buf_workspace(paths)? {
        return fill_pool_from_buf(config, paths, &buf_root).await;
    }

    let random_file_name = format!("{}.desc", uuid::Uuid::new_v4());
    let desc_path = temp_dir().join(random_file_name);

    // HACK: Remove UNC prefix for Windows paths
    let global_import_dir =
        dunce::simplified(config.protoc_include_dir.as_path()).to_string_lossy().to_string();
    let desc_path = dunce::simplified(desc_path.as_path());

    let mut args = vec![
        "--include_imports".to_string(),
        "--include_source_info".to_string(),
        "-I".to_string(),
        global_import_dir,
        "-o".to_string(),
        desc_path.to_string_lossy().to_string(),
    ];

    let mut include_dirs = HashSet::new();
    let mut include_protos = HashSet::new();

    for p in paths {
        if !p.exists() {
            continue;
        }

        // Dirs are added as includes
        if p.is_dir() {
            include_dirs.insert(p.to_string_lossy().to_string());
            continue;
        }

        let parent = p.as_path().parent();
        if let Some(parent_path) = parent {
            match find_parent_proto_dir(parent_path) {
                None => {
                    // Add parent/grandparent as fallback
                    include_dirs.insert(parent_path.to_string_lossy().to_string());
                    if let Some(grandparent_path) = parent_path.parent() {
                        include_dirs.insert(grandparent_path.to_string_lossy().to_string());
                    }
                }
                Some(p) => {
                    include_dirs.insert(p.to_string_lossy().to_string());
                }
            };
        } else {
            debug!("ignoring {:?} since it does not exist.", parent)
        }

        include_protos.insert(p.to_string_lossy().to_string());
    }

    for d in include_dirs.clone() {
        args.push("-I".to_string());
        args.push(d);
    }
    for p in include_protos.clone() {
        args.push(p);
    }

    info!("Invoking protoc with {}", args.join(" "));

    let mut cmd = new_xplatform_command(&config.protoc_bin_path);
    cmd.args(&args);

    let out =
        cmd.output().await.map_err(|e| GenericError(format!("Failed to run protoc: {}", e)))?;

    if !out.status.success() {
        return Err(GenericError(format!(
            "protoc failed with status {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(out.stderr.as_slice())
        )));
    }

    let bytes = fs::read(desc_path).await?;
    let pool = DescriptorPool::decode(editions::lower_file_descriptor_set(&bytes)?.as_slice())?;

    fs::remove_file(desc_path).await?;

    Ok(pool)
}

async fn fill_pool_from_buf(
    config: &GrpcConfig,
    paths: &[PathBuf],
    buf_root: &Path,
) -> Result<DescriptorPool> {
    let desc_path = temp_dir().join(format!("{}.desc", uuid::Uuid::new_v4()));
    let desc_path = dunce::simplified(desc_path.as_path()).to_path_buf();

    let mut cmd = new_xplatform_command(&config.buf_bin_path);
    cmd.current_dir(buf_root)
        .args(["build", ".", "--as-file-descriptor-set", "--output"])
        .arg(&desc_path);

    for path in paths.iter().filter(|p| p.exists()) {
        let canonical = dunce::canonicalize(path)?;
        // Paths were matched against this workspace, so they are always inside it
        let relative = canonical.strip_prefix(buf_root).unwrap_or(&canonical);
        if !relative.as_os_str().is_empty() {
            cmd.arg("--path").arg(relative);
        }
    }

    info!("Invoking buf build in {}", buf_root.display());

    let out = cmd.output().await.map_err(|e| GenericError(format!("Failed to run buf: {}", e)))?;

    if !out.status.success() {
        let _ = fs::remove_file(&desc_path).await;
        return Err(GenericError(format!(
            "buf build failed with status {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    let bytes = fs::read(&desc_path).await?;
    fs::remove_file(&desc_path).await?;
    Ok(DescriptorPool::decode(editions::lower_file_descriptor_set(&bytes)?.as_slice())?)
}

fn find_buf_workspace(paths: &[PathBuf]) -> Result<Option<PathBuf>> {
    let roots: Vec<Option<PathBuf>> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| find_buf_root(if p.is_dir() { p } else { p.parent().unwrap_or(p) }))
        .collect();

    let Some(root) = roots.iter().flatten().next().cloned() else {
        return Ok(None);
    };
    if roots.iter().any(|r| r.as_ref() != Some(&root)) {
        return Err(GenericError(format!(
            "All proto paths must belong to the Buf workspace at {}",
            root.display()
        )));
    }
    Ok(Some(root))
}

fn find_buf_root(start: &Path) -> Option<PathBuf> {
    let start = dunce::canonicalize(start).ok()?;
    let mut nearest_buf_yaml = None;

    for dir in start.ancestors() {
        if dir.join("buf.work.yaml").is_file() {
            return Some(dir.to_path_buf());
        }
        if nearest_buf_yaml.is_none() && dir.join("buf.yaml").is_file() {
            nearest_buf_yaml = Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            break;
        }
    }

    nearest_buf_yaml
}

pub async fn fill_pool_from_reflection(
    uri: &Uri,
    metadata: &BTreeMap<String, String>,
    validate_certificates: bool,
    client_cert: Option<ClientCertificateConfig>,
    max_message_size: usize,
) -> Result<DescriptorPool> {
    let mut pool = DescriptorPool::new();
    let mut client =
        AutoReflectionClient::new(uri, validate_certificates, client_cert, max_message_size)?;

    for service in list_services(&mut client, metadata).await? {
        if service == "grpc.reflection.v1alpha.ServerReflection" {
            continue;
        }
        if service == "grpc.reflection.v1.ServerReflection" {
            continue;
        }
        debug!("Fetching descriptors for {}", service);
        file_descriptor_set_from_service_name(&service, &mut pool, &mut client, metadata).await;
    }

    Ok(pool)
}

async fn list_services(
    client: &mut AutoReflectionClient,
    metadata: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let response =
        client.send_reflection_request(MessageRequest::ListServices("".into()), metadata).await?;

    let list_services_response = match response {
        MessageResponse::ListServicesResponse(resp) => resp,
        MessageResponse::ErrorResponse(e) => {
            return Err(GenericError(format!(
                "Server reflection error listing services: {} ({})",
                e.error_message, e.error_code,
            )));
        }
        _ => return Err(GenericError("Expected a ListServicesResponse variant".to_string())),
    };

    Ok(list_services_response.service.iter().map(|s| s.name.clone()).collect::<Vec<_>>())
}

async fn file_descriptor_set_from_service_name(
    service_name: &str,
    pool: &mut DescriptorPool,
    client: &mut AutoReflectionClient,
    metadata: &BTreeMap<String, String>,
) {
    let response = match client
        .send_reflection_request(
            MessageRequest::FileContainingSymbol(service_name.into()),
            metadata,
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            warn!("Error fetching file descriptor for service {}: {:?}", service_name, e);
            return;
        }
    };

    let file_descriptor_response = match response {
        MessageResponse::FileDescriptorResponse(resp) => resp,
        MessageResponse::ErrorResponse(e) => {
            warn!(
                "Server reflection error for service {}: {} ({})",
                service_name, e.error_message, e.error_code,
            );
            return;
        }
        _ => {
            warn!("Expected a FileDescriptorResponse variant for service {}", service_name);
            return;
        }
    };

    add_file_descriptors_to_pool(
        file_descriptor_response.file_descriptor_proto,
        pool,
        client,
        metadata,
    )
    .await;
}

pub(crate) async fn reflect_types_for_message(
    pool: Arc<RwLock<DescriptorPool>>,
    uri: &Uri,
    json: &str,
    metadata: &BTreeMap<String, String>,
    client_cert: Option<ClientCertificateConfig>,
    max_message_size: usize,
) -> Result<()> {
    // 1. Collect all Any types in the JSON
    let mut extra_types = Vec::new();
    collect_any_types(json, &mut extra_types);

    if extra_types.is_empty() {
        return Ok(()); // nothing to do
    }

    let mut client = AutoReflectionClient::new(uri, false, client_cert, max_message_size)?;
    for extra_type in extra_types {
        {
            let guard = pool.read().await;
            if guard.get_message_by_name(&extra_type).is_some() {
                continue;
            }
        }
        info!("Adding file descriptor for {:?} from reflection", extra_type);
        let req = MessageRequest::FileContainingSymbol(extra_type.clone().into());
        let resp = match client.send_reflection_request(req, metadata).await {
            Ok(r) => r,
            Err(e) => {
                return Err(GenericError(format!(
                    "Error sending reflection request for @type \"{extra_type}\": {e:?}",
                )));
            }
        };
        let files = match resp {
            MessageResponse::FileDescriptorResponse(resp) => resp.file_descriptor_proto,
            MessageResponse::ErrorResponse(e) => {
                warn!(
                    "Server reflection error for @type \"{}\": {} ({})",
                    extra_type, e.error_message, e.error_code,
                );
                continue;
            }
            _ => {
                warn!("Expected a FileDescriptorResponse variant for @type \"{}\"", extra_type);
                continue;
            }
        };

        {
            let mut guard = pool.write().await;
            add_file_descriptors_to_pool(files, &mut *guard, &mut client, metadata).await;
        }
    }

    Ok(())
}

pub(crate) async fn reflect_types_for_dynamic_message(
    pool: Arc<RwLock<DescriptorPool>>,
    uri: &Uri,
    message: &DynamicMessage,
    metadata: &BTreeMap<String, String>,
    client_cert: Option<ClientCertificateConfig>,
    max_message_size: usize,
) -> Result<()> {
    let mut extra_types = HashSet::new();
    collect_any_types_from_dynamic_message(message, &mut extra_types);

    if extra_types.is_empty() {
        return Ok(());
    }

    let mut client = AutoReflectionClient::new(uri, false, client_cert, max_message_size)?;
    for extra_type in extra_types {
        {
            let guard = pool.read().await;
            if guard.get_message_by_name(&extra_type).is_some() {
                continue;
            }
        }
        info!("Adding response file descriptor for {:?} from reflection", extra_type);
        let req = MessageRequest::FileContainingSymbol(extra_type.clone().into());
        let resp = match client.send_reflection_request(req, metadata).await {
            Ok(r) => r,
            Err(e) => {
                return Err(GenericError(format!(
                    "Error sending reflection request for response @type \"{extra_type}\": {e:?}",
                )));
            }
        };
        let files = match resp {
            MessageResponse::FileDescriptorResponse(resp) => resp.file_descriptor_proto,
            MessageResponse::ErrorResponse(e) => {
                warn!(
                    "Server reflection error for response @type \"{}\": {} ({})",
                    extra_type, e.error_message, e.error_code,
                );
                continue;
            }
            _ => {
                warn!(
                    "Expected a FileDescriptorResponse variant for response @type \"{}\"",
                    extra_type,
                );
                continue;
            }
        };

        {
            let mut guard = pool.write().await;
            add_file_descriptors_to_pool(files, &mut *guard, &mut client, metadata).await;
        }
    }

    Ok(())
}

fn collect_any_types_from_dynamic_message(message: &DynamicMessage, out: &mut HashSet<String>) {
    if message.descriptor().full_name() == "google.protobuf.Any" {
        if let Some(Value::String(type_url)) = message.get_field_by_name("type_url").as_deref() {
            if let Some(full_name) = type_url.rsplit_once('/').map(|(_, name)| name) {
                out.insert(full_name.to_string());
            }
        }
    }

    for (_, value) in message.fields() {
        collect_any_types_from_value(value, out);
    }
}

fn collect_any_types_from_value(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::Message(message) => collect_any_types_from_dynamic_message(message, out),
        Value::List(values) => {
            for value in values {
                collect_any_types_from_value(value, out);
            }
        }
        Value::Map(values) => {
            for value in values.values() {
                collect_any_types_from_value(value, out);
            }
        }
        _ => {}
    }
}

#[async_recursion]
pub(crate) async fn add_file_descriptors_to_pool(
    fds: Vec<Vec<u8>>,
    pool: &mut DescriptorPool,
    client: &mut AutoReflectionClient,
    metadata: &BTreeMap<String, String>,
) {
    let mut topo_sort = topology::SimpleTopoSort::new();
    let mut fd_mapping = std::collections::HashMap::with_capacity(fds.len());

    for fd in fds {
        let fdp = match FileDescriptorProto::decode(fd.deref()) {
            Ok(fdp) => fdp,
            Err(e) => {
                warn!("Failed to decode file descriptor: {e}");
                continue;
            }
        };

        topo_sort.insert(fdp.name().to_string(), fdp.dependency.clone());
        fd_mapping.insert(fdp.name().to_string(), fd);
    }

    for node in topo_sort {
        match node {
            Ok(node) => {
                if let Some(fd) = fd_mapping.remove(&node) {
                    let added = editions::lower_file_descriptor_proto(&fd)
                        .and_then(|fd| Ok(pool.decode_file_descriptor_proto(fd.as_slice())?));
                    if let Err(e) = added {
                        warn!("Failed to add file descriptor for {node}: {e}");
                    }
                } else {
                    file_descriptor_set_by_filename(node.as_str(), pool, client, metadata).await;
                }
            }
            Err(_) => {
                warn!("Cycle detected in proto dependencies");
                break;
            }
        }
    }
}

async fn file_descriptor_set_by_filename(
    filename: &str,
    pool: &mut DescriptorPool,
    client: &mut AutoReflectionClient,
    metadata: &BTreeMap<String, String>,
) {
    // We already fetched this file
    if let Some(_) = pool.get_file_by_name(filename) {
        return;
    }

    let msg = MessageRequest::FileByFilename(filename.into());
    let response = client.send_reflection_request(msg, metadata).await;
    let file_descriptor_response = match response {
        Ok(MessageResponse::FileDescriptorResponse(resp)) => resp,
        Ok(MessageResponse::ErrorResponse(e)) => {
            warn!(
                "Server reflection error for {}: {} ({})",
                filename, e.error_message, e.error_code,
            );
            return;
        }
        Ok(_) => {
            warn!("Expected a FileDescriptorResponse variant for {}", filename);
            return;
        }
        Err(e) => {
            warn!("Error fetching file descriptor for {}: {:?}", filename, e);
            return;
        }
    };

    add_file_descriptors_to_pool(
        file_descriptor_response.file_descriptor_proto,
        pool,
        client,
        metadata,
    )
    .await;
}

pub fn method_desc_to_path(md: &MethodDescriptor) -> PathAndQuery {
    let full_name = md.full_name();
    let (namespace, method_name) = full_name
        .rsplit_once('.')
        .ok_or_else(|| anyhow!("invalid method path"))
        .expect("invalid method path");
    PathAndQuery::from_str(&format!("/{}/{}", namespace, method_name)).expect("invalid method path")
}

mod topology {
    use std::collections::{HashMap, HashSet};

    pub struct SimpleTopoSort<T> {
        out_graph: HashMap<T, HashSet<T>>,
        in_graph: HashMap<T, HashSet<T>>,
    }

    impl<T> SimpleTopoSort<T>
    where
        T: Eq + std::hash::Hash + Clone,
    {
        pub fn new() -> Self {
            SimpleTopoSort { out_graph: HashMap::new(), in_graph: HashMap::new() }
        }

        pub fn insert<I: IntoIterator<Item = T>>(&mut self, node: T, deps: I) {
            self.out_graph.entry(node.clone()).or_insert(HashSet::new());
            for dep in deps {
                self.out_graph.entry(node.clone()).or_insert(HashSet::new()).insert(dep.clone());
                self.in_graph.entry(dep.clone()).or_insert(HashSet::new()).insert(node.clone());
            }
        }
    }

    impl<T> IntoIterator for SimpleTopoSort<T>
    where
        T: Eq + std::hash::Hash + Clone,
    {
        type Item = <SimpleTopoSortIter<T> as Iterator>::Item;
        type IntoIter = SimpleTopoSortIter<T>;

        fn into_iter(self) -> Self::IntoIter {
            SimpleTopoSortIter::new(self)
        }
    }

    pub struct SimpleTopoSortIter<T> {
        data: SimpleTopoSort<T>,
        zero_indegree: Vec<T>,
    }

    impl<T> SimpleTopoSortIter<T>
    where
        T: Eq + std::hash::Hash + Clone,
    {
        pub fn new(data: SimpleTopoSort<T>) -> Self {
            let mut zero_indegree = Vec::new();
            for (node, _) in data.in_graph.iter() {
                if !data.out_graph.contains_key(node) {
                    zero_indegree.push(node.clone());
                }
            }
            for (node, deps) in data.out_graph.iter() {
                if deps.is_empty() {
                    zero_indegree.push(node.clone());
                }
            }

            SimpleTopoSortIter { data, zero_indegree }
        }
    }

    impl<T> Iterator for SimpleTopoSortIter<T>
    where
        T: Eq + std::hash::Hash + Clone,
    {
        type Item = Result<T, &'static str>;

        fn next(&mut self) -> Option<Self::Item> {
            if self.zero_indegree.is_empty() {
                if self.data.out_graph.is_empty() {
                    return None;
                }
                return Some(Err("Cycle detected"));
            }

            let node = self.zero_indegree.pop().unwrap();
            if let Some(parents) = self.data.in_graph.get(&node) {
                for parent in parents.iter() {
                    let deps = self.data.out_graph.get_mut(parent).unwrap();
                    deps.remove(&node);
                    if deps.is_empty() {
                        self.zero_indegree.push(parent.clone());
                    }
                }
            }
            self.data.out_graph.remove(&node);

            Some(Ok(node))
        }
    }

    #[test]
    fn test_sort() {
        {
            let mut topo_sort = SimpleTopoSort::new();
            topo_sort.insert("a", []);

            for node in topo_sort {
                match node {
                    Ok(n) => assert_eq!(n, "a"),
                    Err(e) => panic!("err {}", e),
                }
            }
        }

        {
            let mut topo_sort = SimpleTopoSort::new();
            topo_sort.insert("a", ["b"]);
            topo_sort.insert("b", []);

            let mut iter = topo_sort.into_iter();
            match iter.next() {
                Some(Ok(n)) => assert_eq!(n, "b"),
                _ => panic!("err"),
            }
            match iter.next() {
                Some(Ok(n)) => assert_eq!(n, "a"),
                _ => panic!("err"),
            }
            assert_eq!(iter.next(), None);
        }
    }
}

fn find_parent_proto_dir(start_path: impl AsRef<Path>) -> Option<PathBuf> {
    let mut dir = start_path.as_ref().canonicalize().ok()?;

    loop {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            if name == "proto" {
                return Some(dir);
            }
        }

        let parent = dir.parent()?;
        if parent == dir {
            return None; // Reached root
        }

        dir = parent.to_path_buf();
    }
}

#[cfg(test)]
mod buf_tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn temp_workspace() -> PathBuf {
        let dir = temp_dir().join(format!("yaak-buf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Stop the upward config search inside the temp dir
        std::fs::create_dir(dir.join(".git")).unwrap();
        dunce::canonicalize(dir).unwrap()
    }

    #[test]
    fn finds_buf_workspace_root() {
        let root = temp_workspace();
        write(&root.join("buf.work.yaml"), "version: v1\ndirectories: [a, b]\n");
        write(&root.join("a/buf.yaml"), "version: v1\n");
        write(&root.join("a/x.proto"), "");
        write(&root.join("b/buf.yaml"), "version: v1\n");
        write(&root.join("c/y.proto"), "");

        // v1: buf.work.yaml wins over the module's own buf.yaml
        let paths = vec![root.join("a/x.proto"), root.join("b")];
        assert_eq!(find_buf_workspace(&paths).unwrap(), Some(root.clone()));

        // No Buf config at all
        std::fs::remove_file(root.join("buf.work.yaml")).unwrap();
        assert_eq!(find_buf_workspace(&[root.join("c/y.proto")]).unwrap(), None);

        // Mixing Buf and non-Buf paths is rejected
        assert!(find_buf_workspace(&[root.join("a"), root.join("c/y.proto")]).is_err());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn builds_editions_pool_with_buf() {
        let vendored = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates-tauri/yaak-app-client/vendored");
        let config = GrpcConfig {
            protoc_include_dir: vendored.join("protoc/include"),
            protoc_bin_path: vendored.join("protoc/yaakprotoc"),
            buf_bin_path: vendored.join("buf/yaakbuf"),
        };

        let root = temp_workspace();
        write(&root.join("buf.yaml"), "version: v2\nmodules:\n  - path: proto\n");
        write(
            &root.join("proto/acme/common/money.proto"),
            "edition = \"2024\";\npackage acme.common;\nmessage Money { int64 units = 1; }\n",
        );
        write(
            &root.join("proto/acme/v1/shop.proto"),
            r#"edition = "2023";
package acme.v1;
import "acme/common/money.proto";
message Req { acme.common.Money price = 1 [features.message_encoding = DELIMITED]; }
service Shop { rpc Buy(Req) returns (Req); }
"#,
        );

        let pool = fill_pool_from_files(&config, &vec![root.join("proto/acme/v1")]).await.unwrap();
        let price = pool.get_message_by_name("acme.v1.Req").unwrap().get_field_by_name("price");
        assert!(price.unwrap().is_group());
        assert!(pool.get_service_by_name("acme.v1.Shop").is_some());

        std::fs::remove_dir_all(root).unwrap();
    }
}
