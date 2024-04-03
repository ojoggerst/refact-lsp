use std::collections::HashMap;
use std::hash::Hash;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::sync::Mutex as StdMutex;
use std::time::Instant;
use crate::global_context::GlobalContext;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
use ropey::Rope;
use tokio::fs::read_to_string;
use tokio::runtime::Runtime;
use tokio::sync::{RwLock as ARwLock, Mutex as AMutex, RwLock};

use tracing::info;
use url::Url;
use walkdir::WalkDir;
use which::which;

use crate::global_context;
use crate::telemetry;
use crate::vecdb::file_filter::is_valid_file;

#[derive(Debug, Eq, Hash, PartialEq, Clone)]
pub struct Document {
    #[allow(dead_code)]
    pub language_id: String,
    pub text: Rope,
}

impl Document {
    pub fn new(language_id: String, text: Rope) -> Self {
        Self { language_id, text }
    }
}

#[derive(Debug, Clone, Eq)]
pub struct DocumentInfo {
    pub uri: Url,
    pub document: Option<Document>,
}

impl DocumentInfo {
    pub fn new(uri: Url) -> Self {
        Self { uri, document: None }
    }
}

impl PartialEq<Self> for DocumentInfo {
    fn eq(&self, other: &Self) -> bool {
        self.uri == other.uri
    }
}

impl Hash for DocumentInfo {
    fn hash<H>(&self, state: &mut H) where H: std::hash::Hasher {
        self.uri.hash(state);
    }
}

impl DocumentInfo {
    pub fn from_pathbuf(path: &PathBuf) -> Result<Self, String> {
        match pathbuf_to_url(path) {
            Ok(uri) => Ok(Self { uri, document: None }),
            Err(_) => Err("Failed to convert path to URL".to_owned())
        }
    }

    pub fn from_pathbuf_and_text(path: &PathBuf, text: &String) -> Result<Self, String> {
        match pathbuf_to_url(path) {
            Ok(uri) => Ok(Self {
                uri,
                document: Some(Document {
                    language_id: "unknown".to_string(),
                    text: Rope::from_str(&text),
                }),
            }),
            Err(_) => Err("Failed to convert path to URL".to_owned())
        }
    }

    pub fn get_path(&self) -> PathBuf {
        // PathBuf::from(self.uri.path())  -- incorrect code, you can't make a PathBuf from the path path of Url
        self.uri.to_file_path().unwrap_or_default()
    }

    pub async fn read_file(&self) -> io::Result<String> {
        match &self.document {
            Some(doc) => Ok(doc.text.to_string()),
            None => {
                read_to_string(self.get_path()).await
            }
        }
    }

    pub fn read_file_blocked(&self) -> io::Result<String> {
        match &self.document {
            Some(doc) => Ok(doc.text.to_string()),
            None => {
                std::fs::read_to_string(self.get_path())
            }
        }
    }
}


pub struct DocumentsState {
    pub workspace_folders: Arc<StdMutex<Vec<PathBuf>>>,
    pub workspace_files: Arc<StdMutex<Vec<Url>>>,
    // document_map on windows: c%3A/Users/user\Documents/file.ext
    // query on windows: C:/Users/user/Documents/file.ext
    pub document_map: Arc<ARwLock<HashMap<Url, Document>>>,   // if a file is open in IDE and it's outside workspace dirs, it will be in this map and not in workspace_files
    pub cache_dirty: Arc<AMutex<bool>>,
    pub cache_correction: Arc<HashMap<String, String>>,  // map dir3/file.ext -> to /dir1/dir2/dir3/file.ext
    pub cache_fuzzy: Arc<Vec<String>>,                   // slow linear search
    pub fs_watcher: Arc<ARwLock<RecommendedWatcher>>,
}


impl DocumentsState {
    pub fn empty(workspace_dirs: Vec<PathBuf>) -> Self {
        let watcher = RecommendedWatcher::new(|_|{}, Default::default()).unwrap();
        Self {
            workspace_folders: Arc::new(StdMutex::new(workspace_dirs)),
            workspace_files: Arc::new(StdMutex::new(vec![])),
            document_map: Arc::new(ARwLock::new(HashMap::new())),
            cache_dirty: Arc::new(AMutex::<bool>::new(false)),
            cache_correction: Arc::new(HashMap::<String, String>::new()),
            cache_fuzzy: Arc::new(Vec::<String>::new()),
            fs_watcher: Arc::new(ARwLock::new(watcher)),
        }
    }

    pub fn init_watcher(&mut self, gcx: Arc<ARwLock<GlobalContext>>) {
        let gcx_cloned = Arc::downgrade(&gcx.clone());
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let rt = Runtime::new().unwrap();
                rt.block_on(async {
                    if let Ok(event) = res {
                        file_watcher_thread(event, gcx_cloned.clone()).await;
                    }
                })
            },
            Config::default(),
        ).unwrap();
        for folder in self.workspace_folders.lock().unwrap().iter() {
            watcher.watch(folder, RecursiveMode::Recursive).unwrap();
        }
        self.fs_watcher = Arc::new(ARwLock::new(watcher));
    }
}


pub async fn get_file_text_from_memory_or_disk(global_context: Arc<ARwLock<GlobalContext>>, file_path: &String) -> Result<String, String> {
    // if you write pathbuf_to_url(&PathBuf::from(file_path)) without unwrapping it gives: future cannot be sent between threads safe
    let url_mb = pathbuf_to_url(&PathBuf::from(file_path)).map(|x| Some(x)).unwrap_or(None);
    if let Some(url) = url_mb {
        let document_mb = global_context.read().await.documents_state.document_map.read().await.get(&url).cloned();
        if document_mb.is_some() {
            return Ok(document_mb.unwrap().text.to_string());
        }
    }

    let doc_info = match DocumentInfo::from_pathbuf(&PathBuf::from(file_path)) {
        Ok(doc) => doc.read_file().await,
        Err(_) => {
            return Err(format!("cannot parse filepath: {file_path}"));
        }
    };
    doc_info.map_err(|e| e.to_string())
}

pub fn pathbuf_to_url(path: &PathBuf) -> Result<Url, Box<dyn std::error::Error>> {
    let absolute_path = if path.is_absolute() {
        path.clone()
    } else {
        let path = std::env::current_dir()?.join(path);
        path
    };
    let url = Url::from_file_path(absolute_path).map_err(|_| "Failed to convert path to URL")?;
    Ok(url)
}

async fn _run_command(cmd: &str, args: &[&str], path: &PathBuf) -> Option<Vec<PathBuf>> {
    info!("{} EXEC {} {}", path.display(), cmd, args.join(" "));
    let output = async_process::Command::new(cmd)
        .args(args)
        .current_dir(path)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout.clone())
        .ok()
        .map(|s| s.lines().map(|line| path.join(line)).collect())
}

async fn _ls_files_under_version_control(path: &PathBuf) -> Option<Vec<PathBuf>> {
    if path.join(".git").exists() && which("git").is_ok() {
        // Git repository
        _run_command("git", &["ls-files"], path).await
    } else if path.join(".hg").exists() && which("hg").is_ok() {
        // Mercurial repository
        _run_command("hg", &["status", "-c"], path).await
    } else if path.join(".svn").exists() && which("svn").is_ok() {
        // SVN repository
        _run_command("svn", &["list", "-R"], path).await
    } else {
        None
    }
}

const BLACKLISTED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "vendor",
    "build",
    "dist",
    "bin",
    "pkg",
    "lib",
    "lib64",
    "obj",
    "out",
    "venv",
    "env",
    "tmp",
    "temp",
    "logs",
    "coverage",
    "backup"
];

pub fn is_this_inside_blacklisted_dir(path: &PathBuf) -> bool {
    let mut path = path.clone();
    while path.parent().is_some() {
        path = path.parent().unwrap().to_path_buf();
        if let Some(file_name) = path.file_name() {
            if BLACKLISTED_DIRS.contains(&file_name.to_str().unwrap_or_default()) {
                return true;
            }
            if let Some(file_name_str) = file_name.to_str() {
                if file_name_str.starts_with(".") {
                    return true;
                }
            }
        }
    }
    false
}

async fn _ls_files_under_version_control_recursive(path: PathBuf) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = vec![];
    let mut candidates: Vec<PathBuf> = vec![path];
    let mut rejected_reasons: HashMap<String, usize> = HashMap::new();
    let mut blacklisted_dirs_cnt: usize = 0;
    while !candidates.is_empty() {
        let local_path = candidates.pop().unwrap();
        if local_path.is_file() {
            let maybe_valid = is_valid_file(&local_path);
            match maybe_valid {
                Ok(_) => {
                    paths.push(local_path.clone());
                }
                Err(e) => {
                    rejected_reasons.entry(e.to_string()).and_modify(|x| *x += 1).or_insert(1);
                    continue;
                }
            }
        }
        if local_path.is_dir() {
            if BLACKLISTED_DIRS.contains(&local_path.file_name().unwrap().to_str().unwrap()) {
                blacklisted_dirs_cnt += 1;
                continue;
            }
            let maybe_files = _ls_files_under_version_control(&local_path).await;
            if let Some(files) = maybe_files {
                paths.extend(files);
            } else {
                let local_paths: Vec<PathBuf> = WalkDir::new(local_path.clone()).max_depth(1)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_path_buf())
                    .filter(|e| e != &local_path)
                    .collect();
                candidates.extend(local_paths);
            }
        }
    }
    info!("rejected files reasons:");
    for (reason, count) in &rejected_reasons {
        info!("    {:>6} {}", count, reason);
    }
    if rejected_reasons.is_empty() {
        info!("    no bad files at all");
    }
    info!("also the loop bumped into {} blacklisted dirs", blacklisted_dirs_cnt);
    paths
}

pub async fn _retrieve_files_by_proj_folders(proj_folders: Vec<PathBuf>) -> Vec<DocumentInfo> {
    let mut all_files: Vec<DocumentInfo> = Vec::new();
    for proj_folder in proj_folders {
        let files = _ls_files_under_version_control_recursive(proj_folder.clone()).await;
        all_files.extend(files.iter().filter_map(|x| DocumentInfo::from_pathbuf(x).ok()).collect::<Vec<_>>());
    }
    all_files
}

async fn enqueue_files(
    gcx: Arc<ARwLock<GlobalContext>>,
    docs: Vec<DocumentInfo>,
) {
    let (ast_module, vecdb_module) = {
        let cx_locked = gcx.read().await;
        (cx_locked.ast_module.clone(), cx_locked.vec_db.clone())
    };
    match *ast_module.lock().await {
        Some(ref mut ast) => ast.ast_indexer_enqueue_files(&docs, true).await,
        None => {}
    };
    match *vecdb_module.lock().await {
        Some(ref mut db) => db.vectorizer_enqueue_files(&docs, false).await,
        None => {}
    };
}

pub async fn enqueue_all_files_from_workspace_folders(
    gcx: Arc<ARwLock<global_context::GlobalContext>>,
) -> i32 {
    let folders: Vec<PathBuf> = {
        let cx_locked = gcx.read().await;
        let x = cx_locked.documents_state.workspace_folders.lock().unwrap().clone();
        x
    };
    info!("enqueue_all_files_from_workspace_folders started files search with {} folders", folders.len());
    let docs = _retrieve_files_by_proj_folders(folders).await;
    let tmp = docs.iter().map(|x| x.uri.clone()).collect::<Vec<_>>();
    info!("enqueue_all_files_from_workspace_folders found {} files => workspace_files", tmp.len());

    let (ast_module, vecdb_module) = {
        let cx_locked = gcx.write().await;
        {
            *cx_locked.documents_state.cache_dirty.lock().await = true;
        }
        let workspace_files: &mut Vec<Url> = &mut cx_locked.documents_state.workspace_files.lock().unwrap();
        workspace_files.clear();
        workspace_files.extend(tmp);
        (cx_locked.ast_module.clone(), cx_locked.vec_db.clone())
    };
    match *ast_module.lock().await {
        Some(ref mut ast) => ast.ast_indexer_enqueue_files(&docs, false).await,
        None => {
            info!("ast_module is None");
        }
    };
    match *vecdb_module.lock().await {
        Some(ref mut db) => db.vectorizer_enqueue_files(&docs, true).await,
        None => {}
    };
    docs.len() as i32
}

pub async fn on_workspaces_init(
    gcx: Arc<ARwLock<global_context::GlobalContext>>,
) -> i32 {
    enqueue_all_files_from_workspace_folders(gcx.clone()).await
}

pub async fn on_did_open(
    gcx: Arc<ARwLock<global_context::GlobalContext>>,
    file_url: &Url,
    text: &String,
    language_id: &String,
) {
    let doc = Document::new(language_id.clone(), Rope::from_str(&text));
    let (document_map_arc, cache_dirty_arc) = {
        let gcx_locked = gcx.read().await;
        (gcx_locked.documents_state.document_map.clone(), gcx_locked.documents_state.cache_dirty.clone())
    };
    let doc_info = DocumentInfo { uri: file_url.clone(), document: Some(doc.clone()) };
    info!("on_did_open {}", crate::nicer_logs::last_n_chars(&doc_info.get_path().display().to_string(), 30));
    {
        let mut document_map_locked = document_map_arc.write().await;
        document_map_locked.insert(file_url.clone(), doc);
    }
    *(cache_dirty_arc.lock().await) = true;
}

pub async fn on_did_change(
    gcx: Arc<ARwLock<global_context::GlobalContext>>,
    file_url: &Url,
    text: &String,
) {
    let t0 = Instant::now();
    let (document_map_arc, cache_dirty_arc) = {
        let gcx_locked = gcx.read().await;
        (gcx_locked.documents_state.document_map.clone(), gcx_locked.documents_state.cache_dirty.clone())
    };
    let mut mark_dirty: bool = false;
    let doc_info = {
        let mut document_map_locked = document_map_arc.write().await;
        let doc = if document_map_locked.contains_key(file_url) {
            let tmp = document_map_locked.get_mut(file_url).unwrap();
            tmp.text = Rope::from_str(&text);
            tmp.clone()
        } else {
            info!("WARNING: file {} reported changed, but this binary has no record of this file.", crate::nicer_logs::last_n_chars(&file_url.path().to_string(), 30));
            let tmp = &Document::new("unknown".to_owned(), Rope::from_str(&text));
            document_map_locked.insert(file_url.clone(), tmp.clone());
            mark_dirty = true;
            tmp.clone()
        };
        DocumentInfo { uri: file_url.clone(), document: Some(doc.clone()) }
    };
    if mark_dirty {
        *(cache_dirty_arc.lock().await) = true;
    }
    if is_valid_file(&doc_info.get_path()).is_ok() {
        let (ast_module, vecdb_module) = {
            let cx_locked = gcx.read().await;
            (cx_locked.ast_module.clone(), cx_locked.vec_db.clone())
        };
        match *vecdb_module.lock().await {
            Some(ref mut db) => db.vectorizer_enqueue_files(&vec![doc_info.clone()], false).await,
            None => {}
        };
        match *ast_module.lock().await {
            Some(ref mut ast) => ast.ast_indexer_enqueue_files(&vec![doc_info.clone()], false).await,
            None => {}
        };
    }
    telemetry::snippets_collection::sources_changed(
        gcx.clone(),
        &doc_info.uri.to_file_path().unwrap_or_default().to_string_lossy().to_string(),
        text,
    ).await;
    info!("on_did_change {}, total time {:.3}s", crate::nicer_logs::last_n_chars(&file_url.path().to_string(), 30), t0.elapsed().as_secs_f32());
}

pub async fn on_did_delete(
    gcx: Arc<ARwLock<global_context::GlobalContext>>,
    file_url: &Url,
) {
    info!("on_did_delete {}", crate::nicer_logs::last_n_chars(&file_url.path().to_string(), 30));
    let cache_dirty_arc = {
        let gcx_locked = gcx.read().await;
        let document_map = &gcx_locked.documents_state.document_map;
        let mut document_map_locked = document_map.write().await;
        document_map_locked.remove(file_url);
        gcx_locked.documents_state.cache_dirty.clone()
    };
    *(cache_dirty_arc.lock().await) = true;
    let (ast_module, vecdb_module) = {
        let cx_locked = gcx.read().await;
        (cx_locked.ast_module.clone(), cx_locked.vec_db.clone())
    };
    {
        match *vecdb_module.lock().await {
            Some(ref mut db) => {
                let file_path = PathBuf::from(file_url.path());
                db.remove_file(&file_path).await
            }
            None => {}
        };
    }
    {
        match *ast_module.lock().await {
            Some(ref mut ast) => {
                let doc = DocumentInfo {
                    uri: file_url.clone(),
                    document: None,
                };
                ast.remove_file(&doc).await
            }
            None => {}
        };
    }
}

pub async fn add_folder(gcx: Arc<ARwLock<GlobalContext>>, path: &PathBuf) {
    {
        let documents_state = &mut gcx.write().await.documents_state;
        documents_state.workspace_folders.lock().unwrap().push(path.clone());
        let _ = documents_state.fs_watcher.write().await.watch(&path.clone(), RecursiveMode::Recursive);
    }
    let docs = _retrieve_files_by_proj_folders(vec![path.clone()]).await;

    let (ast_module, vecdb_module) = {
        let cx_locked = gcx.read().await;
        (cx_locked.ast_module.clone(), cx_locked.vec_db.clone())
    };
    match *ast_module.lock().await {
        Some(ref mut ast) => ast.ast_indexer_enqueue_files(&docs, false).await,
        None => {}
    };
    match *vecdb_module.lock().await {
        Some(ref mut db) => db.vectorizer_enqueue_files(&docs, false).await,
        None => {}
    };
}

pub async fn remove_folder(gcx: Arc<ARwLock<GlobalContext>>, path: &PathBuf) {
    {
        let documents_state = &mut gcx.write().await.documents_state;
        documents_state.workspace_folders.lock().unwrap().retain(|p| p != path);
        let _ = documents_state.fs_watcher.write().await.unwatch(&path.clone());
    }
    let (ast_module, _vecdb_module) = {
        let cx_locked = gcx.read().await;
        (cx_locked.ast_module.clone(), cx_locked.vec_db.clone())
    };
    match *ast_module.lock().await {
        Some(ref mut ast) => ast.ast_reset_index().await,
        None => {}
    };

    enqueue_all_files_from_workspace_folders(gcx.clone()).await;
}

pub async fn file_watcher_thread(event: Event, gcx: Weak<RwLock<GlobalContext>>) {
    match event.kind {
        EventKind::Any => {},
        EventKind::Access(_) => {},
        EventKind::Create(CreateKind::File) | EventKind::Modify(ModifyKind::Data(DataChange::Content)) => {
            let mut docs: Vec<DocumentInfo> = Vec::new();
            for path in &event.paths {
                if is_this_inside_blacklisted_dir(&path) {
                    continue;
                }
                match is_valid_file(path) {
                    Ok(_) => {
                        docs.push(DocumentInfo::new(pathbuf_to_url(path).unwrap()));
                    },
                    Err(_e) => {
                        // info!("ignoring {} because {}", path.display(), e);
                    }
                }
            }
            if !docs.is_empty() {
                info!("EventKind::Create/Modify {:?}", event.paths);
                if let Some(gcx) = gcx.upgrade() {
                    info!("=> enqueue {} of them", docs.len());
                    if event.kind == EventKind::Create(CreateKind::File) {
                        let tmp = docs.iter().map(|x| x.uri.clone()).collect::<Vec<_>>();
                        gcx.clone().write().await.documents_state.workspace_files.lock().unwrap().extend(tmp);
                    }
                    enqueue_files(gcx, docs).await;
                }
            }
        },
        EventKind::Remove(RemoveKind::File) => {
            let mut never_mind = true;
            for p in &event.paths {
                never_mind &= is_this_inside_blacklisted_dir(&p);
            }
            if !never_mind {
                info!("EventKind::Remove {:?}", event.paths);
                info!("Likely a useful file was removed, rebuild index");
                if let Some(gcx) = gcx.upgrade() {
                    enqueue_all_files_from_workspace_folders(gcx).await;
                }
            }
        },
        EventKind::Other => {}
        _ => {}
    }
}
