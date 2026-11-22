//! Встроенный локальный MTProto-прокси для Telegram (протокол `dd`, как в
//! Flowseal/tg-ws-proxy, MIT). Работает прямо внутри Unkn0WnDPI: никаких
//! внешних exe и отдельных процессов — только потоки внутри программы.
//!
//! Суть: локальный TCP-порт принимает 64-байтовый инициализационный пакет
//! MTProto-obfuscation, расшифровывает его секретом из настроек, устанавливает
//! новое соединение с датацентром Telegram (WebSocket поверх TLS на 443,
//! при неудаче — обычный TCP:443, как fallback в upstream) и ре-шифрует
//! трафик в обе стороны.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aes::cipher::{BlockEncrypt, KeyInit};
use aes::{Aes256, Block};
use base64::Engine;
use native_tls::{TlsConnector, TlsStream};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::logging::Logger;
use crate::paths::Paths;
use crate::settings::Settings;

const HANDSHAKE_LEN: usize = 64;
const SKIP_LEN: usize = 8;
const PREKEY_LEN: usize = 32;
const IV_LEN: usize = 16;
const PROTO_TAG_POS: usize = 56;
const DC_IDX_POS: usize = 60;

const TAG_ABRIDGED: [u8; 4] = [0xef; 4];
const TAG_INTERMEDIATE: [u8; 4] = [0xee; 4];
const TAG_PADDED: [u8; 4] = [0xdd; 4];

/// Обычные (не тестовые) датацентры для TCP-fallback, порт 443.
const DC_IPS: &[(u16, &str)] = &[
    (1, "149.154.175.50"),
    (2, "149.154.167.51"),
    (3, "149.154.175.100"),
    (4, "149.154.167.91"),
    (5, "149.154.171.5"),
    (203, "91.105.192.100"),
];
/// IP, к которым upstream подключается напрямую для WebSocket.
const DIRECT_IPS: &[(u16, &str)] = &[
    (2, "149.154.167.220"),
    (4, "149.154.167.220"),
];
/// Тестовые датацентры.
const TEST_IPS: &[(u16, &str)] = &[
    (1, "149.154.175.10"),
    (2, "149.154.167.40"),
    (3, "149.154.175.117"),
];
/// Домены Cloudflare-прокси upstream (декодированы из его config.py).
/// kwsN.<domain> резолвится через Cloudflare и проксирует на DC Telegram.
const CF_DOMAINS: &[&str] = &[
    "pclead.co.uk",
    "offshor.co.uk",
    "cakeisalie.co.uk",
    "noskomnadzor.co.uk",
    "lovetrue.co.uk",
    "sorokdva.co.uk",
    "pyatdesyatdva.co.uk",
    "kartoshka.co.uk",
    "sorokodin.co.uk",
    "pyatdesyatodin.co.uk",
    "notelega.co.uk",
    "ebally.co.uk",
    "nebally.co.uk",
    "havegreatday.co.uk",
    "pomogite.co.uk",
    "fixtelega.co.uk",
    "sadnews.co.uk",
    "onedaychamp.co.uk",
    "stopblocking.co.uk",
    "nothingthere.co.uk",
];

static RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Abridged,
    Intermediate,
    Padded,
}

#[derive(Clone, Debug)]
pub struct TgStatus {
    pub running: bool,
    pub listening: bool,
    pub external: bool,
    pub pid: u32,
}

// ------------------------------------------------------------------ крипто

/// AES-256-CTR с полным 128-битным счётчиком (как PyCryptodome в upstream).
struct AesCtr {
    cipher: Aes256,
    counter: [u8; 16],
    keystream: [u8; 16],
    pos: usize,
}

impl AesCtr {
    fn new(key: &[u8; 32], iv: &[u8; 16]) -> Self {
        let cipher = Aes256::new_from_slice(key).expect("AES-256 key length");
        let mut c = Self {
            cipher,
            counter: *iv,
            keystream: [0u8; 16],
            pos: 16,
        };
        c.refill();
        c
    }

    fn refill(&mut self) {
        let mut block = Block::from(self.counter);
        self.cipher.encrypt_block(&mut block);
        self.keystream.copy_from_slice(block.as_slice());
        for i in (0..16).rev() {
            let (v, carry) = self.counter[i].overflowing_add(1);
            self.counter[i] = v;
            if !carry {
                break;
            }
        }
        self.pos = 0;
    }

    fn apply(&mut self, data: &mut [u8]) {
        for b in data {
            if self.pos == 16 {
                self.refill();
            }
            *b ^= self.keystream[self.pos];
            self.pos += 1;
        }
    }
}

fn sha256(a: &[u8], b: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(a);
    h.update(b);
    let out = h.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::thread_rng().fill_bytes(&mut out);
    out
}

fn hex_decode(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn random_hex32() -> String {
    let b = random_bytes::<16>();
    let mut s = String::with_capacity(32);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

// -------------------------------------------------------------- handshake

fn try_handshake(hs: &[u8; 64], secret: &[u8; 16]) -> Option<(i16, Tag, Vec<u8>)> {
    let prekey_iv = hs[SKIP_LEN..SKIP_LEN + PREKEY_LEN + IV_LEN].to_vec();
    let key = sha256(&prekey_iv[..PREKEY_LEN], secret);
    let iv: [u8; 16] = prekey_iv[PREKEY_LEN..PREKEY_LEN + IV_LEN].try_into().ok()?;
    let mut dec = AesCtr::new(&key, &iv);
    let mut plain = *hs;
    dec.apply(&mut plain);

    let tag = match &plain[PROTO_TAG_POS..PROTO_TAG_POS + 4] {
        x if x == TAG_ABRIDGED => Tag::Abridged,
        x if x == TAG_INTERMEDIATE => Tag::Intermediate,
        x if x == TAG_PADDED => Tag::Padded,
        _ => return None,
    };
    let dc_idx = i16::from_le_bytes([plain[DC_IDX_POS], plain[DC_IDX_POS + 1]]);
    Some((dc_idx, tag, prekey_iv))
}

fn reserved_start(b: &[u8; 64]) -> bool {
    b[0] == 0xef
        || matches!(&b[0..4], b"HEAD" | b"POST" | b"GET " | b"/ee/" | b"/dd/" | b"OPTI")
        || (b[0] == 0x16 && b[1] == 0x03)
        || (b[4] == 0 && b[5] == 0 && b[6] == 0 && b[7] == 0)
}

fn gen_relay_init(dc_idx: i16, tag: Tag) -> [u8; 64] {
    let tag_bytes = match tag {
        Tag::Abridged => TAG_ABRIDGED,
        Tag::Intermediate => TAG_INTERMEDIATE,
        Tag::Padded => TAG_PADDED,
    };
    loop {
        let rnd = random_bytes::<64>();
        if !reserved_start(&rnd) {
            let orig = rnd;
            let key: &[u8; 32] = orig[8..40].try_into().unwrap();
            let iv: &[u8; 16] = orig[40..56].try_into().unwrap();
            let mut enc = AesCtr::new(key, iv);
            let mut encrypted = orig;
            enc.apply(&mut encrypted);

            let mut tail = [0u8; 8];
            tail[0..4].copy_from_slice(&tag_bytes);
            tail[4..6].copy_from_slice(&dc_idx.to_le_bytes());
            tail[6..8].copy_from_slice(&random_bytes::<2>());

            let mut out = orig;
            for i in 0..8 {
                let keystream = encrypted[56 + i] ^ orig[56 + i];
                out[56 + i] = tail[i] ^ keystream;
            }
            return out;
        }
    }
}

struct Crypto {
    clt_dec: AesCtr,
    tg_enc: AesCtr,
    tg_dec: AesCtr,
    clt_enc: AesCtr,
}

impl Crypto {
    fn new(init: &[u8; 64], secret: &[u8; 16], relay: &[u8; 64]) -> Self {
        let key_in = sha256(&init[8..40], secret);
        let iv_in: [u8; 16] = init[40..56].try_into().unwrap();
        let mut clt_dec = AesCtr::new(&key_in, &iv_in);
        let mut skip = [0u8; 64];
        clt_dec.apply(&mut skip); // инициализационный пакет уже съел 64 байта keystream

        let rev: Vec<u8> = init[8..56].iter().rev().copied().collect();
        let key_out = sha256(&rev[..32], secret);
        let iv_out: [u8; 16] = rev[32..48].try_into().unwrap();
        let clt_enc = AesCtr::new(&key_out, &iv_out);

        let mut tg_enc = AesCtr::new(
            relay[8..40].try_into().unwrap(),
            relay[40..56].try_into().unwrap(),
        );
        tg_enc.apply(&mut skip);

        let rrev: Vec<u8> = relay[8..56].iter().rev().copied().collect();
        let tg_dec = AesCtr::new(
            rrev[..32].try_into().unwrap(),
            rrev[32..48].try_into().unwrap(),
        );

        Self {
            clt_dec,
            tg_enc,
            tg_dec,
            clt_enc,
        }
    }
}

// ---------------------------------------------------------- WS-фреймы/сплит

fn frame(opcode: u8, payload: &[u8], masked: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        out.push((len as u8) | if masked { 0x80 } else { 0 });
    } else if len < 65536 {
        out.push(126 | if masked { 0x80 } else { 0 });
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127 | if masked { 0x80 } else { 0 });
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    if masked {
        let mask = random_bytes::<4>();
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
    } else {
        out.extend_from_slice(payload);
    }
    out
}

fn next_frame(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if buf.len() < 2 {
        return None;
    }
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = (buf[1] & 0x7f) as usize;
    let mut off = 2;
    if len == 126 {
        if buf.len() < 4 {
            return None;
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        off = 4;
    } else if len == 127 {
        if buf.len() < 10 {
            return None;
        }
        len = u64::from_be_bytes(buf[2..10].try_into().ok()?) as usize;
        off = 10;
    }
    let mask = if masked {
        if buf.len() < off + 4 {
            return None;
        }
        let m = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
        off += 4;
        Some(m)
    } else {
        None
    };
    if len > 16 * 1024 * 1024 || buf.len() < off + len {
        return None;
    }
    let mut payload = buf[off..off + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i & 3];
        }
    }
    buf.drain(0..off + len);
    Some((opcode, payload))
}

fn packet_len(buf: &[u8], tag: Tag) -> Option<usize> {
    match tag {
        Tag::Abridged => {
            let n = *buf.first()? & 0x7f;
            if n == 0x7f {
                if buf.len() < 4 {
                    return None;
                }
                let l = (u32::from_le_bytes(buf[1..4].try_into().ok()?) & 0x7fff_ffff) as usize;
                Some(4 + l * 4)
            } else if n == 0 {
                None
            } else {
                Some(1 + n as usize * 4)
            }
        }
        Tag::Intermediate | Tag::Padded => {
            if buf.len() < 4 {
                return None;
            }
            let l = (u32::from_le_bytes(buf[..4].try_into().ok()?) & 0x7fff_ffff) as usize;
            Some(4 + l)
        }
    }
}

/// Режет поток на MTProto-пакеты по открытому тексту, отрезая столько же
/// байт зашифрованного потока (AES-CTR сохраняет длину).
struct Splitter {
    tag: Option<Tag>,
    plain: Vec<u8>,
    cipher: Vec<u8>,
}

impl Splitter {
    fn new(tag: Tag) -> Self {
        Self {
            tag: Some(tag),
            plain: Vec::new(),
            cipher: Vec::new(),
        }
    }

    fn push(&mut self, plain: &[u8], cipher: &[u8], out: &mut Vec<Vec<u8>>) {
        self.plain.extend_from_slice(plain);
        self.cipher.extend_from_slice(cipher);
        loop {
            let len = match self.tag {
                Some(tag) => match packet_len(&self.plain, tag) {
                    Some(0) | None => {
                        if self.plain.len() < 4 {
                            return; // ждём заголовок
                        }
                        // Некорректная длина: дальше пропускаем поток как есть.
                        self.tag = None;
                        out.push(std::mem::take(&mut self.cipher));
                        self.plain.clear();
                        return;
                    }
                    Some(n) => n,
                },
                None => {
                    out.push(std::mem::take(&mut self.cipher));
                    self.plain.clear();
                    return;
                }
            };
            if len > 16 * 1024 * 1024 {
                self.tag = None;
                out.push(std::mem::take(&mut self.cipher));
                self.plain.clear();
                return;
            }
            if self.plain.len() < len {
                return; // пакет ещё не полностью получен
            }
            self.plain.drain(0..len);
            out.push(self.cipher.drain(0..len).collect());
        }
    }
}

// -------------------------------------------------------------- upstream

fn dc_ip(dc: u16, test: bool, direct: bool) -> String {
    let table = if test {
        TEST_IPS
    } else if direct {
        DIRECT_IPS
    } else {
        DC_IPS
    };
    table
        .iter()
        .find(|(id, _)| *id == dc)
        .map(|(_, ip)| (*ip).to_string())
        .unwrap_or_else(|| {
            if test {
                "149.154.167.40".into()
            } else {
                "149.154.167.51".into()
            }
        })
}

struct WsUpstream {
    tls: TlsStream<TcpStream>,
}

impl WsUpstream {
    fn open_tcp(host: &str, direct: Option<SocketAddr>, timeout: Duration) -> Result<TcpStream, String> {
        let tcp = if let Some(addr) = direct {
            TcpStream::connect_timeout(&addr, timeout)
                .map_err(|e| format!("{host}: {e}"))?
        } else {
            let addrs: Vec<SocketAddr> = (host, 443)
                .to_socket_addrs()
                .map_err(|e| format!("DNS {host}: {e}"))?
                .filter(|a| a.is_ipv4())
                .collect();
            let mut last = String::from("нет IPv4-адресов");
            let mut found = None;
            for addr in addrs {
                match TcpStream::connect_timeout(&addr, timeout) {
                    Ok(s) => {
                        found = Some(s);
                        break;
                    }
                    Err(e) => last = e.to_string(),
                }
            }
            found.ok_or_else(|| format!("{host}: {last}"))?
        };
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(timeout));
        let _ = tcp.set_write_timeout(Some(timeout));
        Ok(tcp)
    }

    fn connect_to(host: &str, direct: Option<SocketAddr>, path: &str) -> Result<Self, String> {
        let tcp = Self::open_tcp(host, direct, Duration::from_secs(4))?;
        let connector = TlsConnector::new().map_err(|e| e.to_string())?;
        let mut tls = connector
            .connect(host, tcp)
            .map_err(|e| format!("TLS {host}: {e}"))?;
        let key = base64::engine::general_purpose::STANDARD.encode(random_bytes::<16>());
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n"
        );
        tls.write_all(req.as_bytes())
            .map_err(|e| format!("WebSocket handshake: {e}"))?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        let mut status_ok = false;
        for _ in 0..64 * 1024 {
            if tls.read(&mut byte).map_err(|e| e.to_string())? == 0 {
                break;
            }
            if byte[0] == b'\n' {
                let s = String::from_utf8_lossy(&line).into_owned();
                if s.starts_with("HTTP/1.1 101") || s.starts_with("HTTP/1.0 101") {
                    status_ok = true;
                }
                let empty = s.trim().is_empty();
                line.clear();
                if empty {
                    break;
                }
            } else if byte[0] != b'\r' {
                line.push(byte[0]);
            }
        }
        if !status_ok {
            return Err(format!("{host}: WebSocket-upgrade отклонён"));
        }
        Ok(Self { tls })
    }

    /// Сначала прямой WS к DC, затем домены Cloudflare-прокси upstream
    /// (они доступны там, где заблокированы IP датацентров).
    fn connect(dc: u16, media: bool, test: bool, logger: &Logger) -> Result<Self, String> {
        let path = if test { "/apiws_test" } else { "/apiws" };
        let ip = dc_ip(dc, test, true);
        let direct_host = if media {
            format!("kws{dc}-1.web.telegram.org")
        } else {
            format!("kws{dc}.web.telegram.org")
        };
        let direct_addr: SocketAddr = format!("{ip}:443")
            .parse()
            .map_err(|e| format!("неверный адрес DC: {e}"))?;
        let mut candidates: Vec<(String, Option<SocketAddr>)> =
            vec![(direct_host, Some(direct_addr))];

        // Начинаем со случайного домена, как балансировщик upstream.
        let offset = (random_bytes::<4>()[0] as usize) % CF_DOMAINS.len();
        for i in 0..CF_DOMAINS.len().min(6) {
            let domain = CF_DOMAINS[(offset + i) % CF_DOMAINS.len()];
            candidates.push((format!("kws{dc}.{domain}"), None));
        }

        let mut last = String::new();
        for (host, addr) in candidates {
            match Self::connect_to(&host, addr, path) {
                Ok(ws) => {
                    logger.info(format!("WebSocket к DC {dc} ({host}) установлен"));
                    return Ok(ws);
                }
                Err(e) => last = e,
            }
        }
        Err(format!("не удалось подключиться к DC {dc} через WS: {last}"))
    }
}

// ------------------------------------------------------------- bridges

/// Мост через WebSocket: один поток обслуживает оба направления (native-tls
/// не умеет разделить соединение на две独立ные halves).
fn bridge_ws(
    client: &mut TcpStream,
    ws: &mut WsUpstream,
    crypto: &mut Crypto,
    tag: Tag,
    stop: &AtomicBool,
) {
    let _ = client.set_nonblocking(true);
    let _ = ws.tls.get_ref().set_nonblocking(true);

    let mut splitter = Splitter::new(tag);
    let mut srv_buf: Vec<u8> = Vec::new();
    let mut to_client: Vec<u8> = Vec::new();
    let mut to_server: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16384];
    let mut last_activity = Instant::now();

    loop {
        if !stop.load(Ordering::Relaxed) || last_activity.elapsed() > Duration::from_secs(600) {
            break;
        }
        let mut active = false;

        // клиент -> DC
        match client.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                active = true;
                last_activity = Instant::now();
                let mut plain = chunk[..n].to_vec();
                crypto.clt_dec.apply(&mut plain);
                let mut cipher = plain.clone();
                crypto.tg_enc.apply(&mut cipher);
                let mut packets = Vec::new();
                splitter.push(&plain, &cipher, &mut packets);
                for p in packets {
                    to_server.extend_from_slice(&frame(2, &p, true));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }

        // DC -> клиент
        match ws.tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                active = true;
                last_activity = Instant::now();
                srv_buf.extend_from_slice(&chunk[..n]);
                while let Some((opcode, payload)) = next_frame(&mut srv_buf) {
                    match opcode {
                        0x2 | 0x1 | 0x0 => {
                            let mut data = payload;
                            crypto.tg_dec.apply(&mut data);
                            crypto.clt_enc.apply(&mut data);
                            to_client.extend_from_slice(&data);
                        }
                        0x9 => {
                            let pong = frame(0xA, &payload, true);
                            to_server.extend_from_slice(&pong);
                        }
                        0x8 => return,
                        _ => {}
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(_) => break,
        }

        if !to_client.is_empty() {
            match client.write(&to_client) {
                Ok(0) => break,
                Ok(n) => {
                    to_client.drain(0..n);
                    active = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }
        if !to_server.is_empty() {
            match ws.tls.write(&to_server) {
                Ok(0) => break,
                Ok(n) => {
                    to_server.drain(0..n);
                    active = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }
        if !active {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

/// Простой TCP-fallback (как в upstream): обычный MTProto-obfuscation по 443.
fn bridge_tcp(
    client: TcpStream,
    mut dc_stream: TcpStream,
    mut c2d: (AesCtr, AesCtr),
    mut d2c: (AesCtr, AesCtr),
) {
    let mut a = client;
    let mut b = match dc_stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut a2 = match a.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = a.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = b.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = a2.set_read_timeout(Some(Duration::from_millis(500)));

    let stop_t1 = &RUNNING;
    let t1 = std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        while stop_t1.load(Ordering::Relaxed) {
            match a.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    c2d.0.apply(&mut buf[..n]);
                    c2d.1.apply(&mut buf[..n]);
                    if b.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let t2 = std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        while RUNNING.load(Ordering::Relaxed) {
            match dc_stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    d2c.0.apply(&mut buf[..n]);
                    d2c.1.apply(&mut buf[..n]);
                    if a2.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let _ = t1.join();
    let _ = t2.join();
}

fn handle_client(mut client: TcpStream, secret_hex: String, logger: Logger) {
    let _ = client.set_nodelay(true);
    // Клиенты MTProto шлют 64 байта init сразу; короткий таймаут отсеивает пробы.
    let _ = client.set_read_timeout(Some(Duration::from_secs(3)));
    let mut init = [0u8; HANDSHAKE_LEN];
    if client.read_exact(&mut init).is_err() {
        return;
    }
    let secret = match hex_decode(&secret_hex) {
        Some(s) => s,
        None => return,
    };
    let (dc_idx, tag, _prekey_iv) = match try_handshake(&init, &secret) {
        Some(v) => v,
        None => {
            logger.warn("MTProto: неверный secret или формат handshake");
            return;
        }
    };
    let relay = gen_relay_init(dc_idx, tag);
    let mut crypto = Crypto::new(&init, &secret, &relay);
    let dc = (dc_idx as i32).unsigned_abs() as u16;
    let test = dc >= 10000;
    let media = dc_idx < 0;
    logger.info(format!(
        "MTProto: клиент → DC {dc}{} ({})",
        if test { " [test]" } else { "" },
        match tag {
            Tag::Abridged => "abridged",
            Tag::Intermediate => "intermediate",
            Tag::Padded => "padded",
        }
    ));

    // Пытаемся основной путь: WebSocket поверх TLS к kwsN.web.telegram.org.
    match WsUpstream::connect(dc, media, test, &logger) {
        Ok(mut ws) => {
            let first = frame(2, &relay, true);
            if ws.tls.write_all(&first).is_ok() {
                bridge_ws(&mut client, &mut ws, &mut crypto, tag, &RUNNING);
                return;
            }
        }
        Err(e) => logger.warn(format!("MTProto: {e}; пробуем прямой TCP")),
    }

    // Fallback: обычный TCP:443 к IP датацентра.
    let ip = dc_ip(dc, test, false);
    let addr: SocketAddr = match format!("{ip}:443").parse() {
        Ok(a) => a,
        Err(_) => return,
    };
    let mut dc_stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(8)) {
        Ok(s) => s,
        Err(e) => {
            logger.warn(format!("MTProto: не удалось подключиться к DC {dc}: {e}"));
            return;
        }
    };
    let _ = dc_stream.set_nodelay(true);
    if dc_stream.write_all(&relay).is_err() {
        return;
    }
    let clt_dec = crypto.clt_dec;
    let tg_enc = crypto.tg_enc;
    let tg_dec = crypto.tg_dec;
    let clt_enc = crypto.clt_enc;
    bridge_tcp(
        client,
        dc_stream,
        (clt_dec, tg_enc),
        (tg_dec, clt_enc),
    );
}

// ------------------------------------------------------------------ API

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(150),
    )
    .is_ok()
}

/// Готовит настройки (секрет). Ничего не распаковывается — прокси встроен.
pub fn ensure(_paths: &Paths, settings: &mut Settings, logger: &Logger) -> Result<(), String> {
    if hex_decode(&settings.tg_secret).is_none() {
        settings.tg_secret = random_hex32();
        settings
            .save(&_paths.settings_file)
            .map_err(|e| format!("не удалось сохранить secret: {e}"))?;
        logger.info("Сгенерирован secret встроенного MTProto-прокси");
    }
    Ok(())
}

pub fn link(settings: &Settings) -> String {
    format!(
        "tg://proxy?server=127.0.0.1&port={}&secret=dd{}",
        settings.tg_port, settings.tg_secret
    )
}

pub fn running_here() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

pub fn status(_paths: &Paths, settings: &Settings) -> TgStatus {
    if RUNNING.load(Ordering::Relaxed) {
        return TgStatus {
            running: true,
            listening: true,
            external: false,
            pid: std::process::id(),
        };
    }
    let listening = port_open(settings.tg_port);
    TgStatus {
        running: listening,
        listening,
        external: false,
        pid: 0,
    }
}

pub fn command_path(paths: &Paths) -> std::path::PathBuf {
    paths.config.join("tgws.cmd")
}

pub fn send_command(paths: &Paths, cmd: &str) -> Result<(), String> {
    std::fs::write(command_path(paths), cmd).map_err(|e| e.to_string())
}

pub fn take_command(paths: &Paths) -> Option<String> {
    let p = command_path(paths);
    let cmd = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    let cmd = cmd.trim().to_string();
    if cmd.is_empty() { None } else { Some(cmd) }
}

/// Выполняет команду в текущем процессе (используется стражем).
pub fn apply_command(paths: &Paths, logger: &Logger, cmd: &str) {
    let mut settings = Settings::load(&paths.settings_file);
    let _ = ensure(paths, &mut settings, logger);
    match cmd {
        "start" => {
            if !running_here() && !port_open(settings.tg_port) {
                let _ = start(paths, &settings, logger);
            }
        }
        "stop" => {
            if running_here() {
                stop(paths, logger);
            }
        }
        "restart" => {
            if running_here() {
                stop(paths, logger);
                std::thread::sleep(Duration::from_millis(300));
            }
            let _ = start(paths, &settings, logger);
        }
        _ => {}
    }
}

/// Страж: автозапуск прокси вместе с обходом (если включён в настройках).
pub fn autostart_if_needed(paths: &Paths, logger: &Logger, bypass_running: bool) {
    if !bypass_running {
        return;
    }
    let mut settings = Settings::load(&paths.settings_file);
    if !settings.tg_auto_start {
        return;
    }
    let _ = ensure(paths, &mut settings, logger);
    if !running_here() && !port_open(settings.tg_port) {
        let _ = start(paths, &settings, logger);
    }
}

pub fn start(
    _paths: &Paths,
    settings: &Settings,
    logger: &Logger,
) -> Result<u32, String> {
    if RUNNING.load(Ordering::Relaxed) || port_open(settings.tg_port) {
        return Err("Порт уже занят — прокси уже работает или его держит другая программа".into());
    }
    let listener = TcpListener::bind(("127.0.0.1", settings.tg_port))
        .map_err(|e| format!("не удалось открыть порт {}: {e}", settings.tg_port))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| e.to_string())?;
    let port = settings.tg_port;
    RUNNING.store(true, Ordering::Relaxed);
    let secret = settings.tg_secret.clone();
    let log = logger.clone();

    std::thread::spawn(move || {
        log.info(format!(
            "Встроенный MTProto-прокси слушает 127.0.0.1:{port}",
        ));
        while RUNNING.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((client, _)) => {
                    let s = secret.clone();
                    let l = log.clone();
                    std::thread::spawn(move || handle_client(client, s, l));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
        log.info("Встроенный MTProto-прокси остановлен");
    });

    Ok(std::process::id())
}

pub fn stop(_paths: &Paths, logger: &Logger) {
    if RUNNING.swap(false, Ordering::Relaxed) {
        logger.info("Остановка встроенного MTProto-прокси…");
    }
}

pub fn open_in_telegram(url: &str) -> Result<(), String> {
    crate::winproc::open_url(url);
    Ok(())
}

pub fn copy_link(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{LocalAlloc, LocalLock, LocalUnlock, LMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("буфер обмена недоступен".into());
        }
        EmptyClipboard();
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let h = LocalAlloc(LMEM_MOVEABLE, units.len() * 2);
        if h.is_null() {
            CloseClipboard();
            return Err("не удалось выделить память для буфера обмена".into());
        }
        let p = LocalLock(h) as *mut u16;
        if p.is_null() {
            CloseClipboard();
            return Err("не удалось скопировать ссылку".into());
        }
        std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
        LocalUnlock(h);
        SetClipboardData(CF_UNICODETEXT, h as _);
        CloseClipboard();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_init(secret: &[u8; 16], tag: [u8; 4], dc: i16) -> [u8; 64] {
        let prekey = [0x22u8; 32];
        let iv = [0x33u8; 16];
        let key = sha256(&prekey, secret);
        let mut hs = [0u8; 64];
        hs[0..8].copy_from_slice(&[0x44; 8]);
        hs[8..40].copy_from_slice(&prekey);
        hs[40..56].copy_from_slice(&iv);
        hs[56..60].copy_from_slice(&tag);
        hs[60..62].copy_from_slice(&dc.to_le_bytes());
        // prekey и IV передаются открытым текстом; шифруется только хвост [56..64].
        let mut enc = AesCtr::new(&key, &iv);
        enc.apply(&mut [0u8; 56]);
        enc.apply(&mut hs[56..64]);
        hs
    }

    #[test]
    fn handshake_decodes_tag_and_dc() {
        let secret = [0x11u8; 16];
        let hs = make_init(&secret, TAG_PADDED, -2);
        let (dc, tag, pre_iv) = try_handshake(&hs, &secret).unwrap();
        assert_eq!(dc, -2);
        assert_eq!(tag, Tag::Padded);
        assert_eq!(&pre_iv[..32], &[0x22u8; 32]);
        assert_eq!(&pre_iv[32..], &[0x33u8; 16]);
    }

    #[test]
    fn wrong_secret_rejected() {
        let hs = make_init(&[0x11u8; 16], TAG_PADDED, 2);
        assert!(try_handshake(&hs, &[0x99u8; 16]).is_none());
    }

    #[test]
    fn relay_init_tail_decodes() {
        let relay = gen_relay_init(-2, Tag::Intermediate);
        let key: &[u8; 32] = relay[8..40].try_into().unwrap();
        let iv: &[u8; 16] = relay[40..56].try_into().unwrap();
        let mut dec = AesCtr::new(key, iv);
        dec.apply(&mut [0u8; 56]); // инициализационный пакет съел 56 байт keystream
        let mut tail = [0u8; 8];
        tail.copy_from_slice(&relay[56..64]);
        dec.apply(&mut tail);
        assert_eq!(&tail[0..4], &TAG_INTERMEDIATE);
        assert_eq!(i16::from_le_bytes([tail[4], tail[5]]), -2);
    }

    #[test]
    fn hex_secret_is_32_chars() {
        let s = random_hex32();
        assert_eq!(s.len(), 32);
        assert!(hex_decode(&s).is_some());
    }
}
