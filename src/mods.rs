//! Universal Mod & Plugin Runtime for Soundya.
//! Supervises mod processes, routes dynamic API calls over UNIX domain sockets,
//! and serves static frontend assets (JS/CSS) on Soundya's native port.

use std::{
    collections::HashMap,
    ffi::CString,
    fs,
    io::{self, Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::json;

pub const PRIMARY_MODS_DIR: &str = "/data/opt/soundya/mods";
pub const ALT_MODS_DIR: &str = "/data/opt/lumya/mods";

fn ensure_dir_p(path: &Path) {
    if let Some(parent) = path.parent() {
        if !crate::fsx::is_dir(parent) {
            ensure_dir_p(parent);
        }
    }
    crate::fsx::ensure_dir(path);
}

fn make_executable(path: &Path) {
    if let Ok(c_path) = CString::new(path.to_string_lossy().as_bytes()) {
        unsafe {
            libc::chmod(c_path.as_ptr(), 0o755);
        }
    }
}

pub fn mods_dir() -> PathBuf {
    let p = Path::new(PRIMARY_MODS_DIR);
    if crate::fsx::exists(p) {
        return p.to_path_buf();
    }
    let alt = Path::new(ALT_MODS_DIR);
    if crate::fsx::exists(alt) {
        return alt.to_path_buf();
    }
    p.to_path_buf()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FrontendConfig {
    #[serde(default)]
    pub scripts: Vec<String>,
    #[serde(default)]
    pub styles: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendConfig {
    #[serde(default, alias = "bin")]
    pub exec: String,
    #[serde(default = "default_socket")]
    pub socket: String,
    #[serde(default)]
    pub routes: Vec<String>,
}

fn default_socket() -> String {
    "mod.sock".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModManifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub frontend: FrontendConfig,
    #[serde(default)]
    pub backend: Option<BackendConfig>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub enabled: bool,
    pub running: bool,
    pub frontend: FrontendConfig,
    pub has_backend: bool,
}

pub struct ModManager {
    dir: PathBuf,
    active_config: Mutex<HashMap<String, bool>>,
    manifests: Mutex<HashMap<String, ModManifest>>,
    processes: Mutex<HashMap<String, Child>>,
}

static MANAGER: OnceLock<Arc<ModManager>> = OnceLock::new();

pub fn init() -> Arc<ModManager> {
    MANAGER.get_or_init(|| {
        let mgr = Arc::new(ModManager::new(mods_dir()));
        mgr.rescan();
        mgr
    }).clone()
}

pub fn get_manager() -> Option<Arc<ModManager>> {
    MANAGER.get().cloned()
}

impl ModManager {
    pub fn new(dir: PathBuf) -> Self {
        ensure_dir_p(&dir);
        let config_file = dir.join("mods.json");
        let active: HashMap<String, bool> = crate::fsx::read_string(&config_file)
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        Self {
            dir,
            active_config: Mutex::new(active),
            manifests: Mutex::new(HashMap::new()),
            processes: Mutex::new(HashMap::new()),
        }
    }

    fn save_config(&self) {
        let path = self.dir.join("mods.json");
        if let Ok(cfg) = self.active_config.lock() {
            if let Ok(json_str) = serde_json::to_string_pretty(&*cfg) {
                let _ = fs::write(path, json_str);
            }
        }
    }

    pub fn rescan(&self) {
        let mut new_manifests = HashMap::new();
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                if name.starts_with('.') {
                    continue;
                }

                if crate::fsx::is_dir(&path) {
                    let manifest_path = path.join("manifest.json");
                    if crate::fsx::is_file(&manifest_path) {
                        if let Some(s) = crate::fsx::read_string(&manifest_path) {
                            if let Ok(mut m) = serde_json::from_str::<ModManifest>(&s) {
                                if m.id.is_empty() {
                                    m.id = name.to_string();
                                }
                                new_manifests.insert(m.id.clone(), m);
                            }
                        }
                    }
                } else if crate::fsx::is_file(&path) {
                    // Loose .js / .css mod backward compatibility
                    if name.ends_with(".js") {
                        let id = name.trim_end_matches(".js").to_string();
                        if !new_manifests.contains_key(&id) && !new_manifests.contains_key(name) {
                            let m = ModManifest {
                                id: name.to_string(),
                                name: name.to_string(),
                                version: "1.0.0".to_string(),
                                description: "Пользовательский скрипт".to_string(),
                                author: "".to_string(),
                                frontend: FrontendConfig {
                                    scripts: vec![format!("/mods/{name}")],
                                    styles: vec![],
                                },
                                backend: None,
                            };
                            new_manifests.insert(name.to_string(), m);
                        }
                    } else if name.ends_with(".css") {
                        let id = name.trim_end_matches(".css").to_string();
                        if !new_manifests.contains_key(&id) && !new_manifests.contains_key(name) {
                            let m = ModManifest {
                                id: name.to_string(),
                                name: name.to_string(),
                                version: "1.0.0".to_string(),
                                description: "Пользовательские стили".to_string(),
                                author: "".to_string(),
                                frontend: FrontendConfig {
                                    scripts: vec![],
                                    styles: vec![format!("/mods/{name}")],
                                },
                                backend: None,
                            };
                            new_manifests.insert(name.to_string(), m);
                        }
                    }
                }
            }
        }

        {
            let mut m_guard = self.manifests.lock().unwrap();
            *m_guard = new_manifests;
        }

        self.sync_backends();
    }

    pub fn sync_backends(&self) {
        let (manifests, config) = {
            let m = self.manifests.lock().unwrap().clone();
            let c = self.active_config.lock().unwrap().clone();
            (m, c)
        };

        for (id, manifest) in &manifests {
            let is_enabled = config.get(id).copied().unwrap_or(false);
            if is_enabled {
                if manifest.backend.is_some() {
                    self.ensure_backend_running(manifest);
                }
            } else {
                self.stop_backend(id);
            }
        }

        // Clean up any process whose mod no longer exists
        let mut proc_guard = self.processes.lock().unwrap();
        let running_ids: Vec<String> = proc_guard.keys().cloned().collect();
        for id in running_ids {
            if !manifests.contains_key(&id) || !config.get(&id).copied().unwrap_or(false) {
                if let Some(mut child) = proc_guard.remove(&id) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    }

    fn ensure_backend_running(&self, manifest: &ModManifest) {
        let Some(backend) = &manifest.backend else { return };
        let mut proc_guard = self.processes.lock().unwrap();

        if let Some(child) = proc_guard.get_mut(&manifest.id) {
            match child.try_wait() {
                Ok(None) => return, // Still running happily!
                _ => {
                    // Process exited or failed, remove so we restart it
                    proc_guard.remove(&manifest.id);
                }
            }
        }

        let mod_dir = self.dir.join(&manifest.id);
        let exec_path = mod_dir.join(&backend.exec);
        if !crate::fsx::is_file(&exec_path) {
            eprintln!("mods: cannot start backend for {}: file {:?} not found", manifest.id, exec_path);
            return;
        }

        make_executable(&exec_path);

        let sock_path = mod_dir.join(&backend.socket);
        if crate::fsx::exists(&sock_path) {
            let _ = fs::remove_file(&sock_path);
        }

        eprintln!("mods: starting backend for mod «{}» (exec: {:?}, sock: {:?})", manifest.id, exec_path, sock_path);
        match Command::new(&exec_path)
            .current_dir(&mod_dir)
            .env("SOUNDYA_MOD_ID", &manifest.id)
            .env("SOUNDYA_MOD_DIR", &mod_dir)
            .env("SOUNDYA_MOD_SOCKET", &sock_path)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                proc_guard.insert(manifest.id.clone(), child);
            }
            Err(e) => {
                eprintln!("mods: failed to spawn backend for {}: {e}", manifest.id);
            }
        }
    }

    pub fn stop_backend(&self, id: &str) {
        let mut proc_guard = self.processes.lock().unwrap();
        if let Some(mut child) = proc_guard.remove(id) {
            eprintln!("mods: stopping backend for mod «{id}»");
            let _ = child.kill();
            let _ = child.wait();
        }

        // Clean up socket
        let sock_path = self.dir.join(id).join("mod.sock");
        if crate::fsx::exists(&sock_path) {
            let _ = fs::remove_file(&sock_path);
        }
    }

    pub fn toggle_mod(&self, id: &str, enabled: bool) -> bool {
        {
            let mut cfg = self.active_config.lock().unwrap();
            cfg.insert(id.to_string(), enabled);
        }
        self.save_config();
        self.sync_backends();
        true
    }

    pub fn delete_mod(&self, id: &str) -> bool {
        self.stop_backend(id);
        {
            let mut cfg = self.active_config.lock().unwrap();
            cfg.remove(id);
        }
        self.save_config();

        let mod_dir = self.dir.join(id);
        if crate::fsx::is_dir(&mod_dir) {
            crate::fsx::remove_tree(&mod_dir);
        } else if crate::fsx::is_file(&mod_dir) {
            let _ = fs::remove_file(&mod_dir);
        }
        self.rescan();
        true
    }

    pub fn reset_all(&self) {
        // 1. Stop and wait for all running mod processes
        {
            let mut proc_guard = self.processes.lock().unwrap();
            for (id, mut child) in proc_guard.drain() {
                eprintln!("mods: stopping backend process for «{id}» (emergency reset)");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        // 2. Clear all active mod configurations
        {
            let mut cfg = self.active_config.lock().unwrap();
            cfg.clear();
        }
        self.save_config();
        self.rescan();
    }

    pub fn list_mods(&self) -> Vec<ModInfo> {
        self.rescan();
        let manifests = self.manifests.lock().unwrap().clone();
        let config = self.active_config.lock().unwrap().clone();
        let mut proc_guard = self.processes.lock().unwrap();

        let mut list = Vec::new();
        for (id, m) in manifests {
            let enabled = config.get(&id).copied().unwrap_or(false);
            let has_backend = m.backend.is_some();
            let mut running = false;

            if let Some(child) = proc_guard.get_mut(&id) {
                if let Ok(None) = child.try_wait() {
                    running = true;
                }
            }

            // Normalise frontend paths
            let mut fe = m.frontend.clone();
            fe.scripts = fe.scripts.into_iter().map(|s| {
                if s.starts_with('/') { s } else { format!("/mods/{id}/{s}") }
            }).collect();
            fe.styles = fe.styles.into_iter().map(|s| {
                if s.starts_with('/') { s } else { format!("/mods/{id}/{s}") }
            }).collect();

            list.push(ModInfo {
                id,
                name: m.name,
                version: m.version,
                description: m.description,
                author: m.author,
                enabled,
                running,
                frontend: fe,
                has_backend,
            });
        }

        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn list_mods_json(&self) -> String {
        let mods = self.list_mods();
        let active = self.active_config.lock().unwrap().clone();
        json!({
            "mods": mods,
            "active": active
        }).to_string()
    }

    /// Finds which active mod socket should handle this API path.
    pub fn find_socket_for_route(&self, path: &str) -> Option<(PathBuf, String)> {
        let clean_path = path.split('?').next().unwrap_or(path);
        let manifests = self.manifests.lock().unwrap().clone();
        let config = self.active_config.lock().unwrap().clone();

        for (id, m) in manifests {
            if !config.get(&id).copied().unwrap_or(false) {
                continue;
            }
            let Some(backend) = &m.backend else { continue };
            let sock_path = self.dir.join(&id).join(&backend.socket);

            // 1. Direct path /api/mods/<id>/*
            let prefix = format!("/api/mods/{id}");
            if clean_path == prefix || clean_path.starts_with(&format!("{prefix}/")) {
                let subpath = clean_path.strip_prefix(&prefix).unwrap_or("/").to_string();
                return Some((sock_path, if subpath.is_empty() { "/".to_string() } else { subpath }));
            }

            // 2. Custom route alias (e.g. /api/live, /api/catalog, /api/state)
            for r in &backend.routes {
                if clean_path == r || clean_path.starts_with(&format!("{r}/")) || clean_path.starts_with(&format!("{r}?")) {
                    return Some((sock_path, path.to_string()));
                }
            }
        }
        None
    }

    /// Proxies an HTTP request to a mod's UNIX domain socket and streams the response back.
    pub fn proxy_unix(
        &self,
        sock_path: &Path,
        method: &str,
        path: &str,
        head_raw: &str,
        body: &str,
        conn: &TcpStream,
    ) -> io::Result<()> {
        let mut sock = None;
        for _ in 0..10 {
            if let Ok(s) = UnixStream::connect(sock_path) {
                sock = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let Some(mut sock) = sock else {
            let mut out = conn;
            write!(
                out,
                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nerror: mod backend socket unavailable"
            )?;
            return Ok(());
        };

        // Write HTTP Request to the mod's UNIX socket
        write!(sock, "{method} {path} HTTP/1.1\r\n")?;
        // Pass original headers
        write!(sock, "{head_raw}")?;
        if !head_raw.ends_with("\r\n\r\n") {
            write!(sock, "\r\n")?;
        }
        if !body.is_empty() {
            sock.write_all(body.as_bytes())?;
        }
        sock.flush()?;

        // Stream response from mod socket to TCP connection (supports SSE & large streams)
        let mut out = conn;
        let mut buf = [0u8; 8192];
        loop {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Serves static assets for mods: /mods/<mod_id>/<file> or /mods/<loose_file>
    pub fn serve_static(&self, rel_path: &str, method: &str) -> Option<(&'static str, &'static str, Vec<u8>)> {
        let raw_clean = rel_path.trim_start_matches('/').split('?').next().unwrap_or(rel_path);
        if raw_clean.contains("..") || raw_clean.starts_with('/') {
            return Some(("403 Forbidden", "text/plain", "forbidden".as_bytes().to_vec()));
        }

        let file_path = self.dir.join(raw_clean);
        if !crate::fsx::is_file(&file_path) {
            return None;
        }

        let mime = if raw_clean.ends_with(".css") {
            "text/css; charset=utf-8"
        } else if raw_clean.ends_with(".js") {
            "application/javascript; charset=utf-8"
        } else if raw_clean.ends_with(".json") {
            "application/json; charset=utf-8"
        } else if raw_clean.ends_with(".png") {
            "image/png"
        } else if raw_clean.ends_with(".svg") {
            "image/svg+xml"
        } else if raw_clean.ends_with(".jpg") || raw_clean.ends_with(".jpeg") {
            "image/jpeg"
        } else {
            "text/plain; charset=utf-8"
        };

        if method == "HEAD" {
            return Some(("200 OK", mime, Vec::new()));
        }

        match crate::fsx::read_bytes(&file_path) {
            Ok(bytes) => Some(("200 OK", mime, bytes)),
            Err(_) => Some(("500 Internal Server Error", "text/plain", "failed to read file".as_bytes().to_vec())),
        }
    }

    /// Handles upload of mod packages (.tar, .tar.gz, .js, .css, .json)
    pub fn handle_upload(&self, filename: &str, bytes: &[u8]) -> Result<String, String> {
        let clean_name = Path::new(filename).file_name().and_then(|n| n.to_str()).unwrap_or("");
        if clean_name.is_empty() {
            return Err("invalid filename".into());
        }

        let mod_id = clean_name
            .trim_end_matches(".zip")
            .trim_end_matches(".tar.gz")
            .trim_end_matches(".tgz")
            .trim_end_matches(".tar");
        if !mod_id.is_empty() {
            self.stop_backend(mod_id);
            let _ = fs::remove_file(self.dir.join(mod_id).join(format!("{mod_id}-backend")));
            let _ = fs::remove_file(self.dir.join(mod_id).join("backend"));
        }

        if clean_name.ends_with(".zip") {
            let temp_zip = self.dir.join(".upload.zip");
            fs::write(&temp_zip, bytes).map_err(|e| e.to_string())?;

            let status = Command::new("unzip")
                .arg("-o")
                .arg(&temp_zip)
                .arg("-d")
                .arg(&self.dir)
                .status()
                .map_err(|e| format!("failed to unpack zip: {e}"))?;

            let _ = fs::remove_file(temp_zip);

            if !status.success() {
                return Err("zip extraction failed".into());
            }

            // If the archive extracted directly into self.dir without a subfolder:
            let root_manifest = self.dir.join("manifest.json");
            if crate::fsx::exists(&root_manifest) && !mod_id.is_empty() {
                let mod_sub = self.dir.join(mod_id);
                ensure_dir_p(&mod_sub);
                let _ = fs::rename(&root_manifest, mod_sub.join("manifest.json"));
                for name in [
                    "backend",
                    &format!("{mod_id}-backend"),
                    &format!("{mod_id}.js"),
                    &format!("{mod_id}.css"),
                ] {
                    let loose = self.dir.join(name);
                    if crate::fsx::exists(&loose) {
                        let _ = fs::rename(&loose, mod_sub.join(name));
                    }
                }
            }

            self.rescan();
            Ok("archive unpacked successfully".into())
        } else if clean_name.ends_with(".tar") || clean_name.ends_with(".tar.gz") || clean_name.ends_with(".tgz") {
            let temp_tar = self.dir.join(".upload.tar");
            fs::write(&temp_tar, bytes).map_err(|e| e.to_string())?;

            let status = Command::new("tar")
                .arg("-xf")
                .arg(&temp_tar)
                .arg("-C")
                .arg(&self.dir)
                .status()
                .map_err(|e| format!("failed to unpack tar: {e}"))?;

            let _ = fs::remove_file(temp_tar);

            if !status.success() {
                return Err("tar extraction failed".into());
            }

            // If the archive extracted directly into self.dir without a subfolder:
            let root_manifest = self.dir.join("manifest.json");
            if crate::fsx::exists(&root_manifest) && !mod_id.is_empty() {
                let mod_sub = self.dir.join(mod_id);
                ensure_dir_p(&mod_sub);
                let _ = fs::rename(&root_manifest, mod_sub.join("manifest.json"));
                for name in [
                    "backend",
                    &format!("{mod_id}-backend"),
                    &format!("{mod_id}.js"),
                    &format!("{mod_id}.css"),
                ] {
                    let loose = self.dir.join(name);
                    if crate::fsx::exists(&loose) {
                        let _ = fs::rename(&loose, mod_sub.join(name));
                    }
                }
            }

            self.rescan();
            Ok("archive unpacked successfully".into())
        } else {
            let target = self.dir.join(clean_name);
            fs::write(&target, bytes).map_err(|e| e.to_string())?;
            self.rescan();
            Ok(format!("file «{clean_name}» uploaded successfully"))
        }
    }
}

pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for &b in s.as_bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

pub fn inject_loader(html: &str) -> String {
    const LOADER_SCRIPT: &str = r#"<script>
// ==================== Universal Mod & Plugin Loader ====================
let allInstalledMods = [];
let activeModsConfig = {};

function injectModsCard() {
  const settingsTab = document.getElementById('tab-settings');
  if (!settingsTab) return;

  // 1. Add "Моды" into Settings subnav (same row with Соединение, Аккаунт, Колонка, etc.)
  const subnav = settingsTab.querySelector('.subnav');
  if (subnav && !document.getElementById('subnav-mods-btn')) {
    const subBtn = document.createElement('button');
    subBtn.id = 'subnav-mods-btn';
    subBtn.type = 'button';
    subBtn.dataset.sub = 'mods';
    subBtn.textContent = 'Моды';
    subBtn.onclick = () => selectModsSub();
    subnav.appendChild(subBtn);

    subnav.querySelectorAll('button:not(#subnav-mods-btn)').forEach(b => {
      b.addEventListener('click', () => {
        const sub = document.getElementById('sub-mods');
        if (sub) sub.classList.remove('on');
        subBtn.classList.remove('on');
      });
    });
  }

  // 2. Create dedicated .sub[data-sub="mods"] inside #tab-settings
  if (!document.getElementById('sub-mods')) {
    const subMods = document.createElement('div');
    subMods.className = 'sub';
    subMods.id = 'sub-mods';
    subMods.dataset.sub = 'mods';
    subMods.innerHTML = createModsCardHtml();
    settingsTab.appendChild(subMods);
  }
}

function selectModsSub() {
  document.querySelectorAll('#tab-settings .subnav button').forEach(b => {
    b.classList.toggle('on', b.id === 'subnav-mods-btn' || b.dataset.sub === 'mods');
  });
  document.querySelectorAll('#tab-settings .sub').forEach(s => {
    s.classList.toggle('on', s.id === 'sub-mods' || s.dataset.sub === 'mods');
  });
  try { localStorage.setItem('soundya-sub', 'mods'); } catch (e) {}
}

function createModsCardHtml() {
  return `
    <h2 class="section" style="margin-top:0;padding-top:0;border-top:0">Моды и дополнения</h2>
    <p class="hint">Установка и управление автономными плагинами (матрица Станции 2, оформление, пульт ТВ, телеметрия). Модули работают на лету без перекомпиляции Soundya.</p>
    
    <div style="background: rgba(255,255,255,0.03); border: 1px solid var(--line); border-radius: 14px; padding: 14px 16px; margin: 12px 0; width: 100%; box-sizing: border-box;">
      <div style="display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:10px;">
        <div>
          <b style="font-size:15px; color:var(--ink);">Сборка Soundya (форк Gemeur)</b>
          <div class="hint" style="font-size:13px; margin-top:2px;">Репозиторий обновлений: <a href="https://github.com/Gemeur/soundya" target="_blank" rel="noopener" style="color:var(--accent); text-decoration:none;">github.com/Gemeur/soundya</a></div>
        </div>
        <div style="display:flex; gap:8px; align-items:center; flex-wrap:wrap;">
          <button type="button" class="sm" onclick="checkUpdate()" id="btnCheckFirmware">Проверить на GitHub</button>
          <label class="btn sm" style="cursor:pointer; display:inline-flex; align-items:center; background:rgba(255,255,255,0.08); border:1px solid var(--line); border-radius:10px; padding:6px 12px; font-weight:600;">
            <span>Загрузить soundya-armv7.tar</span>
            <input type="file" accept=".tar" style="display:none;" onchange="uploadFirmwareTar(this)">
          </label>
        </div>
      </div>
      <div class="msg" id="firmwareMsg" style="margin-top:8px;"></div>
    </div>

    <div class="mods-container" style="background: rgba(255,255,255,0.03); border: 1px solid var(--line); border-radius: 14px; padding: 16px; margin: 12px 0; width: 100%; box-sizing: border-box;">
      <div style="font-size:14px; font-weight:600; margin-bottom:10px; color:var(--ink);">Установленные плагины и моды:</div>
      <div id="modsList" class="mods-list" style="display:flex; flex-direction:column; gap:10px; width: 100%; box-sizing: border-box;">
        <div class="hint">Загрузка дополнений…</div>
      </div>
      <div style="margin-top: 16px; padding-top: 12px; border-top: 1px solid var(--line); display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:10px;">
        <label class="btn sm" style="cursor:pointer; display:inline-flex; align-items:center; gap:6px; background:var(--accent); color:var(--accent-ink); border-radius:10px; padding:8px 14px; font-weight:600;">
          <span>+ Загрузить пакет мода (.tar / .zip / .js)</span>
          <input type="file" accept=".tar,.tar.gz,.tgz,.zip,.js,.css" style="display:none;" onchange="uploadModFile(this)">
        </label>
        <button type="button" class="sm" onclick="loadMods(true)">Обновить список</button>
      </div>
      <div class="msg mods-msg" id="modsMsg"></div>
    </div>
  `;
}

async function uploadFirmwareTar(input) {
  const file = input.files && input.files[0];
  if (!file) return;
  const msg = document.getElementById('firmwareMsg');
  if (msg) {
    msg.textContent = 'Отправляю прошивку на колонку (5–7 МБ)…';
    msg.className = 'msg';
  }
  try {
    const res = await fetch('/api/update/upload', {
      method: 'POST',
      body: file
    });
    const text = await res.text();
    if (msg) {
      msg.textContent = text;
      msg.className = text.startsWith('error') ? 'msg err' : 'msg ok';
    }
  } catch (err) {
    if (msg) {
      msg.textContent = 'Ошибка загрузки: ' + err.message;
      msg.className = 'msg err';
    }
  }
  input.value = '';
}

async function loadMods(forceRefresh = false) {
  try {
    injectModsCard();
  } catch (e) {
    console.warn('Error injecting mods card:', e);
  }

  try {
    const res = await fetch('/api/mods');
    if (!res.ok) return;
    const data = await res.json();
    allInstalledMods = data.mods || [];
    activeModsConfig = data.active || {};

    const list = document.getElementById('modsList');
    if (list) {
      if (allInstalledMods.length === 0) {
        list.innerHTML = '<p class="hint" style="margin:4px 0;">В папке /data/opt/soundya/mods/ пока нет установленных модов. Вы можете загрузить архив (.tar/.zip) или скрипт через кнопку ниже.</p>';
      } else {
        list.innerHTML = allInstalledMods.map(m => {
          const isChecked = m.enabled ? 'checked' : '';
          const backendBadge = m.has_backend
            ? (m.running ? ' <span style="font-size:11px; color:#2f9e5b; font-weight:600;">● сервис запущен</span>' : ' <span style="font-size:11px; color:var(--muted);">○ сервис выключен</span>')
            : '';
          const verBadge = m.version ? `<span style="font-size:11px; opacity:0.7;">v${m.version}</span>` : '';
          return `
            <div style="display:flex; justify-content:space-between; align-items:flex-start; padding:12px; border-radius:10px; background:rgba(255,255,255,0.03); border:1px solid var(--line); width:100%; box-sizing:border-box; gap:12px;">
              <label class="check" style="display:flex; align-items:flex-start; gap:10px; margin:0; flex:1 1 0%; min-width:0; cursor:pointer;">
                <input type="checkbox" data-mod="${m.id}" ${isChecked} onchange="toggleMod('${m.id}', this.checked)" style="flex-shrink:0; margin-top:3px;">
                <span style="flex:1 1 0%; min-width:0; display:block;">
                  <b style="font-size:15px;">${m.name}</b> ${verBadge} ${backendBadge}<br>
                  <span class="hint" style="font-size:13px; display:block; word-break:normal; overflow-wrap:break-word;">${m.description || m.id}</span>
                </span>
              </label>
              <button type="button" class="sm ghost" onclick="deleteMod('${m.id}')" title="Удалить" style="opacity:0.6; padding:4px 8px; flex-shrink:0;">✕</button>
            </div>
          `;
        }).join('');
      }
    }

    for (const m of allInstalledMods) {
      if (m.enabled) {
        attachMod(m);
      } else {
        detachMod(m);
      }
    }
  } catch (e) {
    console.warn('Error loading mods:', e);
  }
}

function attachMod(mod) {
  const fe = mod.frontend || {};
  (fe.styles || []).forEach((href, idx) => {
    const id = `mod-css-${mod.id}-${idx}`;
    if (!document.getElementById(id)) {
      const link = document.createElement('link');
      link.id = id;
      link.rel = 'stylesheet';
      link.href = href + (href.includes('?') ? '&' : '?') + 'v=' + Date.now();
      document.head.appendChild(link);
    }
  });

  (fe.scripts || []).forEach((src, idx) => {
    const id = `mod-js-${mod.id}-${idx}`;
    if (!document.getElementById(id)) {
      const script = document.createElement('script');
      script.id = id;
      script.src = src + (src.includes('?') ? '&' : '?') + 'v=' + Date.now();
      document.body.appendChild(script);
    }
  });
}

function detachMod(mod) {
  const fe = mod.frontend || {};
  (fe.styles || []).forEach((_, idx) => {
    const link = document.getElementById(`mod-css-${mod.id}-${idx}`);
    if (link) link.remove();
  });

  (fe.scripts || []).forEach((_, idx) => {
    const script = document.getElementById(`mod-js-${mod.id}-${idx}`);
    if (script) {
      const hookName = 'destroyMod_' + mod.id.replace(/[^a-zA-Z0-9]/g, '_');
      if (typeof window[hookName] === 'function') {
        try { window[hookName](); } catch (e) {}
      }
      script.remove();
    }
  });
}

async function toggleMod(id, enable) {
  try {
    await fetch('/api/mods/toggle', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id, enabled: enable })
    });
  } catch (e) {}

  const mod = allInstalledMods.find(m => m.id === id);
  if (mod) {
    mod.enabled = enable;
    if (enable) attachMod(mod); else detachMod(mod);
  }

  const msg = document.getElementById('modsMsg');
  if (msg) {
    msg.className = 'msg on mods-msg';
    msg.textContent = enable ? `Мод «${mod ? mod.name : id}» включён` : `Мод «${mod ? mod.name : id}» отключён`;
    setTimeout(() => { if (msg) msg.className = 'msg mods-msg'; }, 2500);
  }
}

async function deleteMod(id) {
  if (!confirm(`Удалить дополнение «${id}»?`)) return;
  try {
    await fetch('/api/mods/delete', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id })
    });
    await loadMods(true);
  } catch (e) {}
}

async function uploadModFile(input) {
  if (!input.files || !input.files[0]) return;
  const file = input.files[0];
  const msg = document.getElementById('modsMsg');
  if (msg) {
    msg.className = 'msg on mods-msg';
    msg.textContent = `Чтение файла «${file.name}»…`;
  }

  try {
    const reader = new FileReader();
    reader.onload = async () => {
      if (msg) msg.textContent = `Загрузка пакета «${file.name}» на колонку…`;
      try {
        const base64 = reader.result.split(',')[1];
        const res = await fetch('/api/mods/upload', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ filename: file.name, base64: base64 })
        });
        if (res.ok) {
          if (msg) msg.textContent = `Пакет «${file.name}» успешно загружен и установлен!`;
          input.value = '';
          setTimeout(() => loadMods(true), 150);
        } else {
          const err = await res.text();
          if (msg) msg.textContent = `Ошибка загрузки: ${err}`;
        }
      } catch (e) {
        if (msg) msg.textContent = `Сбой сети: ${e.message || e}`;
      }
    };
    reader.onerror = () => {
      if (msg) msg.textContent = 'Ошибка чтения локального файла';
    };
    reader.readAsDataURL(file);
  } catch (e) {
    if (msg) msg.textContent = `Ошибка: ${e.message || e}`;
  }
}

async function emergencyResetMods() {
  if (!confirm('Отключить все установленные моды и вернуть Soundya в чистый стоковый режим (Debug Reset)?')) return;
  try {
    await fetch('/api/mods/reset', { method: 'POST' });
  } catch (e) {
    console.warn('Reset error:', e);
  }
  try {
    localStorage.removeItem('tab');
    localStorage.removeItem('soundya-sub');
  } catch (e) {}
  window.location.href = window.location.pathname + '?reset=' + Date.now();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', () => {
    loadMods();
    try {
      if (localStorage.getItem('soundya-sub') === 'mods') selectModsSub();
    } catch (e) {}
  });
} else {
  setTimeout(() => {
    loadMods();
    try {
      if (localStorage.getItem('soundya-sub') === 'mods') selectModsSub();
    } catch (e) {}
  }, 50);
}
</script>"#;

    let debug_bar = r#"<div id="soundya-debug-bar" style="position:fixed!important;bottom:12px!important;right:12px!important;z-index:2147483647!important;display:flex!important;gap:8px!important;align-items:center!important;pointer-events:auto!important;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif!important;">
  <button id="soundya-reset-mods-btn" type="button" title="Аварийный сброс всех модов (Debug Mode)" style="background:#d32f2f!important;color:#ffffff!important;border:1px solid rgba(255,255,255,0.45)!important;border-radius:999px!important;padding:7px 15px!important;font-size:12px!important;font-weight:700!important;box-shadow:0 4px 16px rgba(0,0,0,0.55)!important;cursor:pointer!important;opacity:0.92!important;transition:opacity .15s,transform .15s!important;white-space:nowrap!important;outline:none!important;" onmouseover="this.style.opacity='1';this.style.transform='scale(1.03)'" onmouseout="this.style.opacity='0.92';this.style.transform='scale(1)'" onclick="emergencyResetMods()">
    ⚡ Сбросить моды (Debug Mode)
  </button>
</div>"#;

    let payload = format!("{debug_bar}\n{LOADER_SCRIPT}");
    if html.contains("</body>") {
        html.replace("</body>", &format!("{payload}\n</body>"))
    } else {
        format!("{html}\n{payload}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_deserialization() {
        let json_str = r#"{
  "id": "lumya",
  "name": "Подсветка Станции 2",
  "version": "1.0.0",
  "description": "88-LED матрица",
  "author": "Gemeur",
  "frontend": {
    "scripts": ["lumya.js"],
    "styles": ["lumya.css"]
  },
  "backend": {
    "exec": "lumya-backend",
    "socket": "mod.sock",
    "routes": ["/api/live"]
  }
}"#;
        let res = serde_json::from_str::<ModManifest>(json_str);
        assert!(res.is_ok(), "Failed to deserialize: {:?}", res.err());
        let m = res.unwrap();
        assert_eq!(m.id, "lumya");
        assert_eq!(m.name, "Подсветка Станции 2");
        assert!(m.backend.is_some());
        assert_eq!(m.backend.unwrap().exec, "lumya-backend");
    }
}
