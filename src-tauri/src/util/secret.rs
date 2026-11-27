//! 单一敏感值的本机密文：当前只有 `[llm].api_key` 走这里。
//!
//! 目标很窄 —— **文件被拷到别的机器上就读不出内容**。`app.toml` 其余部分仍是明文
//! （工作目录、模型、思考强度都没有保密需要，用户还要能手改），只有 api_key 的值
//! 以 `enc:v1:<base64>` 的形式存放。
//!
//! 机制：本机标识(Windows `MachineGuid` / Linux `/etc/machine-id` /
//! macOS `IOPlatformUUID`) → SHA-256 派生 256 位密钥 → AES-256-GCM 加密，
//! 每次随机 12 字节 nonce，tag 由 GCM 自带，不需要单独存。
//!
//! 能防什么、不能防什么：
//! - ✅ 配置被误传（网盘 / Git / 发给同事）：没有本机标识就解不开。
//! - ❌ 同一台机器上的恶意程序：派生密钥的输入就在本机，能读文件就能读标识。
//!   这一层挡不住，也不属于本模块的职责。
//!
//! **换主板 / 重装系统会让机器标识变化，旧密文解不开** —— 这是本方案的固有代价。
//! 调用方必须把「解不开」当作「未配置」处理并提示重填，绝不能让启动失败。
//!
//! 只做「一个字符串进、一个字符串出」，不认识 api_key 是什么、也不读写文件。

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use tracing::warn;

/// 密文前缀：既是「这是密文」的判据，也带格式版本（将来换算法能认出老数据）。
pub const PREFIX: &str = "enc:v1:";

/// 密钥派生的域分离串：同一个机器标识在这套算法下只产出一个密钥，
/// 将来别的用途要用同一机器标识派生时，换个域串即可，不会撞车。
const DOMAIN: &[u8] = b"hi-agent/llm-api-key/v1:";

/// GCM 推荐的 nonce 长度。
const NONCE_LEN: usize = 12;

/// 本机标识读一次就够（启动时一次文件/注册表读，之后走缓存）。
static MACHINE: OnceLock<String> = OnceLock::new();

/// 文本是否为本模块产出的密文。
pub fn is_sealed(text: &str) -> bool {
    text.trim_start().starts_with(PREFIX)
}

/// 加密：返回带 `enc:v1:` 前缀的密文，可直接写进 app.toml。
pub fn seal(plain: &str) -> anyhow::Result<String> {
    seal_with(plain, &derive_key(&machine_id()))
}

/// 解密：成功返回明文；**任何失败都返回 `None`**（换了机器、数据被改、格式不认识）。
///
/// 故意不区分失败原因：调用方只有一种正确反应 —— 当作没配置过，
/// 让用户重新填一次。把错误细节抛给用户既帮不上忙，也容易让人误以为需要修文件。
pub fn open(text: &str) -> Option<String> {
    open_with(text, &derive_key(&machine_id()))
}

// ============ 实现 ============

fn seal_with(plain: &str, key: &[u8; 32]) -> anyhow::Result<String> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|e| anyhow::anyhow!("初始化加密器失败: {e}"))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plain.as_bytes())
        .map_err(|_| anyhow::anyhow!("加密失败"))?;

    // nonce 直接拼在密文前面：解密方按固定长度切开即可，格式自描述、无需额外字段
    let mut raw = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    raw.extend_from_slice(nonce.as_slice());
    raw.extend_from_slice(&ciphertext);
    Ok(format!("{PREFIX}{}", BASE64.encode(raw)))
}

fn open_with(text: &str, key: &[u8; 32]) -> Option<String> {
    let body = text.trim().strip_prefix(PREFIX)?;
    let raw = BASE64.decode(body).ok()?;
    // 至少要有 nonce + 一个 tag，否则连切都切不出来
    if raw.len() <= NONCE_LEN {
        return None;
    }
    let (nonce, ciphertext) = raw.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let plain = cipher.decrypt(Nonce::from_slice(nonce), ciphertext).ok()?;
    String::from_utf8(plain).ok()
}

fn derive_key(machine: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(machine.as_bytes());
    hasher.finalize().into()
}

/// 本机标识（进程内只读一次）。
fn machine_id() -> String {
    MACHINE
        .get_or_init(|| {
            platform_id().unwrap_or_else(|| {
                warn!("读不到本机标识，api_key 加密退化为弱标识（迁移或重装后可能解不开）");
                fallback_id()
            })
        })
        .clone()
}

/// 兜底标识：机器标识读不到时至少保证「同一台机器上稳定」，
/// 弱于真标识，但不至于让整个功能不可用。
fn fallback_id() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let home = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!("fallback:{host}:{home}")
}

#[cfg(windows)]
fn platform_id() -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let subkey = wide(r"SOFTWARE\Microsoft\Cryptography");
    let name = wide("MachineGuid");
    let mut buf = [0u16; 128];
    // pcbdata 是「入参=缓冲区字节数，出参=写入字节数（含结尾 NUL）」
    let mut size = std::mem::size_of_val(&buf) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if rc != 0 {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1).min(buf.len());
    let value = String::from_utf16_lossy(&buf[..len]).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(target_os = "linux")]
fn platform_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "macos")]
fn platform_id() -> Option<String> {
    // macOS 没有 machine-id 文件，硬件 UUID 只能从 IOKit 拿；ioreg 是系统自带命令行，
    // 启动时跑一次即可。行形如：`"IOPlatformUUID" = "0A1B2C3D-..."`
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("IOPlatformUUID"))
        .and_then(|l| l.rsplit('"').nth(1))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_id() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        derive_key("test-machine-0001")
    }

    #[test]
    fn roundtrip_hides_plaintext() {
        let sealed = seal_with("sk-abcdef123456", &key()).expect("加密应成功");
        assert!(sealed.starts_with(PREFIX));
        assert!(is_sealed(&sealed));
        assert!(!sealed.contains("sk-abcdef"), "密文里不能出现明文片段");
        assert_eq!(
            open_with(&sealed, &key()).as_deref(),
            Some("sk-abcdef123456")
        );
    }

    /// 换机器（机器标识不同）必须解不开——这是整个方案的意义所在。
    #[test]
    fn another_machine_cannot_open() {
        let sealed = seal_with("sk-abcdef123456", &key()).expect("加密应成功");
        assert_eq!(open_with(&sealed, &derive_key("another-machine")), None);
    }

    /// 密文被改动一个字符（含 GCM tag）就必须拒绝，不能返回半截明文。
    #[test]
    fn tampered_ciphertext_is_rejected() {
        let sealed = seal_with("sk-abcdef123456", &key()).expect("加密应成功");
        let mut chars: Vec<char> = sealed.chars().collect();
        let idx = chars.len() - 3; // 避开可能存在的 base64 padding
        chars[idx] = if chars[idx] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert_eq!(open_with(&tampered, &key()), None);
    }

    #[test]
    fn plaintext_is_not_sealed_and_not_openable() {
        assert!(!is_sealed("sk-abcdef123456"));
        assert_eq!(open_with("sk-abcdef123456", &key()), None);
        assert_eq!(open_with("enc:v1:", &key()), None);
        assert_eq!(open_with("enc:v1:not-base64!!", &key()), None);
    }

    /// 每次加密 nonce 不同：同一个 key 加密同一个值，密文也必须不同。
    #[test]
    fn nonce_makes_ciphertext_unique() {
        let a = seal_with("sk-same", &key()).expect("加密应成功");
        let b = seal_with("sk-same", &key()).expect("加密应成功");
        assert_ne!(a, b);
        assert_eq!(open_with(&a, &key()).as_deref(), Some("sk-same"));
        assert_eq!(open_with(&b, &key()).as_deref(), Some("sk-same"));
    }

    /// 走真实机器标识的端到端往返（上面几条用的是测试固定 key，不覆盖平台读取代码）。
    /// 含非 ASCII 与首尾空格无关的普通字符，确认字节层面没有编码问题。
    #[test]
    fn end_to_end_with_this_machine() {
        let plain = "sk-e2e-测试-0123456789";
        let sealed = seal(plain).expect("加密应成功");
        assert!(is_sealed(&sealed));
        assert_eq!(open(&sealed).as_deref(), Some(plain));
    }

    /// Windows / macOS 的本机标识都是系统自带项，读不到就意味着加密退化成弱标识
    /// （换机后仍可能解开），属于必须尽早暴露的故障。Linux 各发行版差异大（容器里常缺
    /// `/etc/machine-id`），只走兜底，不断言。
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn platform_id_is_available() {
        assert!(
            platform_id().is_some(),
            "本机标识读取失败，api_key 加密会退化为弱标识"
        );
    }
}
