use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    },
};

const PREFIX: &str = "dpapi-v1:";

/// Queue URLs and session headers are bound to the current Windows user.
///
/// Takes the plaintext by value and overwrites its bytes with volatile zero writes right
/// after the DPAPI call, whether sealing succeeded or failed, so the caller's buffer does
/// not outlive this function. Mirrors `unseal`, which zeroes the decrypted buffer.
pub fn seal(value: String) -> Result<String> {
    let mut bytes = value.into_bytes();
    let len: u32 = bytes.len().try_into().context("Kuyruk kaydı çok büyük")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: len,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let description: Vec<u16> = "SSDownload queue\0".encode_utf16().collect();
    let ok = unsafe {
        CryptProtectData(
            &input,
            description.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    let error = (ok == 0).then(std::io::Error::last_os_error);
    wipe(&mut bytes);
    if let Some(error) = error {
        return Err(error).context("Kuyruk verisi Windows DPAPI ile korunamadı");
    }
    let encoded = unsafe {
        STANDARD.encode(std::slice::from_raw_parts(
            output.pbData,
            output.cbData as usize,
        ))
    };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(format!("{PREFIX}{encoded}"))
}

pub fn unseal(value: &str) -> Result<String> {
    let Some(encoded) = value.strip_prefix(PREFIX) else {
        bail!("Desteklenmeyen veya korunmamış kuyruk kaydı");
    };
    let mut bytes = STANDARD.decode(encoded).context("Kuyruk kaydı bozuk")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into()?,
        pbData: bytes.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("Kuyruk kaydı açılamadı; kayıt bu Windows kullanıcısına ait olmayabilir");
    }
    let result = unsafe {
        String::from_utf8(
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec(),
        )
    };
    unsafe {
        for index in 0..output.cbData as usize {
            std::ptr::write_volatile(output.pbData.add(index), 0);
        }
        LocalFree(output.pbData.cast());
    }
    wipe(&mut bytes);
    result.context("Kuyruk kaydı geçerli UTF-8 değil")
}

/// Zeroes a buffer with volatile writes the optimizer cannot remove as dead stores.
fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
