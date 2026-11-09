use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use ring::digest::{Context, SHA256};
use ring::signature::{ED25519, UnparsedPublicKey};
use semver::Version;
use serde_json::Value;

use super::RELEASES_URL;

pub(super) const MANIFEST_NAME: &str = "argos-update.json";
pub(super) const INSTALLER_NAME: &str = "ARGOS-Setup.exe";

const MAX_MANIFEST_SIZE: u64 = 4 * 1024;
const MAX_INSTALLER_SIZE: u64 = 32 * 1024 * 1024;

pub(super) struct Release {
    pub(super) version: Version,
    size: u64,
    sha256: [u8; 32],
    manifest: Vec<u8>,
}

impl Release {
    pub(super) fn same_artifact(&self, other: &Self) -> bool {
        self.version == other.version && self.size == other.size && self.sha256 == other.sha256
    }
}

pub(super) fn discover(current: &Version) -> Result<Option<Release>, String> {
    let public_key = public_key()?;
    let url = format!("{RELEASES_URL}/latest/download/{MANIFEST_NAME}");
    let Some(response) = get(&url)? else {
        return Ok(None);
    };
    let release = verify_manifest(read_manifest(response.into_reader())?, &public_key)?;
    Ok(
        (release.version.pre.is_empty() && release.version.cmp_precedence(current).is_gt())
            .then_some(release),
    )
}

pub(super) fn download(release: &Release, directory: &Path) -> Result<(), String> {
    let url = format!(
        "{RELEASES_URL}/download/v{}/{INSTALLER_NAME}",
        release.version
    );
    let response = get(&url)?.ok_or("O instalador da versão anunciada não foi encontrado.")?;
    let installer = directory.join(INSTALLER_NAME);
    write_new(&installer, |file| {
        verify_installer(release, response.into_reader(), file)
    })?;

    // A presença do manifesto completo indica que o instalador já foi baixado e validado.
    let result = write_new(&directory.join(MANIFEST_NAME), |file| {
        file.write_all(&release.manifest)
            .map_err(|error| format!("Não foi possível gravar o manifesto da atualização. {error}"))
    });
    if result.is_err() {
        let _ = fs::remove_file(installer);
    }
    result
}

pub(super) fn staged(directory: &Path) -> Result<Release, String> {
    let public_key = public_key()?;
    let manifest = File::open(directory.join(MANIFEST_NAME))
        .map_err(|error| format!("Não foi possível abrir o manifesto da atualização. {error}"))?;
    let release = verify_manifest(read_manifest(manifest)?, &public_key)?;
    let installer = File::open(directory.join(INSTALLER_NAME))
        .map_err(|error| format!("Não foi possível abrir o instalador da atualização. {error}"))?;
    verify_installer(&release, installer, io::sink())?;
    Ok(release)
}

fn public_key() -> Result<[u8; 32], String> {
    let value = option_env!("ARGOS_UPDATE_PUBLIC_KEY")
        .ok_or("A verificação de atualizações não está configurada nesta compilação do ARGOS.")?;
    lowercase_hex(value)
        .then(|| decode_hex(value))
        .flatten()
        .ok_or_else(|| "A chave pública de atualização do ARGOS é inválida.".into())
}

fn get(url: &str) -> Result<Option<ureq::Response>, String> {
    let agent = ureq::AgentBuilder::new()
        .https_only(true)
        .redirects(5)
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build();
    match agent
        .get(url)
        .set("User-Agent", concat!("ARGOS/", env!("CARGO_PKG_VERSION")))
        .call()
    {
        Ok(response) if response.status() == 200 => Ok(Some(response)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Ok(response) => Err(format!(
            "Resposta inesperada do GitHub (HTTP {}).",
            response.status()
        )),
        Err(ureq::Error::Status(status, _)) => Err(format!("O GitHub retornou HTTP {status}.")),
        Err(ureq::Error::Transport(error)) => Err(format!(
            "Não foi possível acessar as atualizações do ARGOS ({:?}).",
            error.kind()
        )),
    }
}

fn read_manifest(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_MANIFEST_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Não foi possível ler o manifesto da atualização. {error}"))?;
    if bytes.len() as u64 > MAX_MANIFEST_SIZE {
        return Err("O manifesto da atualização excede o tamanho permitido.".into());
    }
    Ok(bytes)
}

fn verify_manifest(manifest: Vec<u8>, public_key: &[u8; 32]) -> Result<Release, String> {
    const INVALID: &str = "O manifesto da atualização é inválido.";
    let parsed: Value = serde_json::from_slice(&manifest).map_err(|_| INVALID)?;
    let fields = parsed
        .as_object()
        .filter(|fields| fields.len() == 4)
        .ok_or(INVALID)?;
    let version = fields
        .get("version")
        .and_then(Value::as_str)
        .ok_or(INVALID)?;
    let size = fields
        .get("size")
        .and_then(Value::as_u64)
        .filter(|size| (1..=MAX_INSTALLER_SIZE).contains(size))
        .ok_or(INVALID)?;
    let hash = fields
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|hash| lowercase_hex(hash))
        .ok_or(INVALID)?;
    let sha256 = decode_hex::<32>(hash).ok_or(INVALID)?;
    let signature = fields
        .get("signature")
        .and_then(Value::as_str)
        .and_then(decode_hex::<64>)
        .ok_or(INVALID)?;
    let version = Version::parse(version).map_err(|_| INVALID)?;

    let canonical = format!("ARGOS-UPDATE-v1\n{version}\n{size}\n{hash}\n");
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(canonical.as_bytes(), &signature)
        .map_err(|_| "A assinatura da atualização do ARGOS não confere.")?;
    Ok(Release {
        version,
        size,
        sha256,
        manifest,
    })
}

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    hex::decode_to_slice(value, &mut bytes).ok()?;
    Some(bytes)
}

fn verify_installer(
    release: &Release,
    reader: impl Read,
    mut writer: impl Write,
) -> Result<(), String> {
    let mut reader = reader.take(release.size + 1);
    let mut buffer = [0; 16 * 1024];
    let mut received = 0;
    let mut hash = Context::new(&SHA256);
    loop {
        let count = reader.read(&mut buffer).map_err(|error| {
            format!("Não foi possível ler o instalador da atualização. {error}")
        })?;
        if count == 0 {
            break;
        }
        received += count as u64;
        if received > release.size {
            return Err("O instalador da atualização excede o tamanho anunciado.".into());
        }
        let chunk = &buffer[..count];
        hash.update(chunk);
        writer.write_all(chunk).map_err(|error| {
            format!("Não foi possível gravar o instalador da atualização. {error}")
        })?;
    }
    if received != release.size || hash.finish().as_ref() != release.sha256.as_slice() {
        return Err("O instalador da atualização não corresponde ao manifesto assinado.".into());
    }
    Ok(())
}

fn write_new(
    path: &Path,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Não foi possível preparar o arquivo da atualização. {error}"))?;
    let result = write(&mut file).and_then(|()| {
        file.sync_all()
            .map_err(|error| format!("Não foi possível salvar o arquivo da atualização. {error}"))
    });
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::digest::digest;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    const INSTALLER: &[u8] = b"installer fixture";

    fn fixture() -> (Value, [u8; 32]) {
        let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let size = INSTALLER.len();
        let sha256 = hex::encode(digest(&SHA256, INSTALLER));
        let signed = format!("ARGOS-UPDATE-v1\n1.4.0\n{size}\n{sha256}\n");
        let manifest = json!({
            "version": "1.4.0",
            "size": size,
            "sha256": sha256,
            "signature": hex::encode(key.sign(signed.as_bytes())),
        });
        (manifest, key.public_key().as_ref().try_into().unwrap())
    }

    #[test]
    fn verifies_signed_release_and_rejects_tampering() {
        let (manifest, key) = fixture();
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let release = verify_manifest(bytes.clone(), &key).unwrap();
        assert_eq!(release.version, Version::new(1, 4, 0));
        assert!(verify_manifest(bytes, &[1; 32]).is_err());

        for (field, value) in [
            ("version", json!("1.4.1")),
            ("size", json!(INSTALLER.len() - 1)),
            ("sha256", json!("ab".repeat(32))),
            ("signature", json!("ab".repeat(64))),
        ] {
            let mut tampered = manifest.clone();
            tampered[field] = value;
            assert!(verify_manifest(serde_json::to_vec(&tampered).unwrap(), &key).is_err());
        }
    }

    #[test]
    fn rejects_truncated_oversize_and_modified_installer_bytes() {
        let (manifest, key) = fixture();
        let release = verify_manifest(serde_json::to_vec(&manifest).unwrap(), &key).unwrap();
        let mut saved = Vec::new();
        verify_installer(&release, INSTALLER, &mut saved).unwrap();
        assert_eq!(saved, INSTALLER);
        let changed = vec![b'x'; INSTALLER.len()];
        let oversize = vec![0; INSTALLER.len() + 1];
        for bad in [
            &INSTALLER[..INSTALLER.len() - 1],
            &changed[..],
            &oversize[..],
        ] {
            assert!(verify_installer(&release, bad, io::sink()).is_err());
        }
    }

    #[test]
    fn bounds_the_manifest_and_declared_installer_size() {
        let oversized = vec![0; MAX_MANIFEST_SIZE as usize + 1];
        assert!(read_manifest(oversized.as_slice()).is_err());

        let (mut manifest, key) = fixture();
        manifest["size"] = json!(MAX_INSTALLER_SIZE + 1);
        assert!(verify_manifest(serde_json::to_vec(&manifest).unwrap(), &key).is_err());
    }
}
