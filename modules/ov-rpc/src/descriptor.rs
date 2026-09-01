//! # 服务描述符（服务发现用）
//!
//! [`define_service!`](crate::define_service) 在编译期从同一张方法表生成
//! 描述符字节（单一真相源，无第二份表可漂移），经保留 method id 0
//! （[`crate::METHOD_INIT`]）的 INIT 请求回发，多块时走大响应通路
//! （>255B 字节流帧，见 crate 文档"消息尺寸分层"）。
//!
//! ## 紧凑线格式（v1）
//!
//! ```text
//! [proto u8][desc_len varint][count varint]     —— 头（desc_len = 条目区字节数）
//! count × [mid varint][flags u8][name_len varint][name utf8]
//! ```
//!
//! - varint = LEB128（7bit/组 + 续传位）；
//! - flags：bit0 one-way（send/urgent）、bit1 urgent、bit2 acall；
//! - **desc_len 是硬需求**：单块响应在通道上是定长 255B（尾随零填充），
//!   裸载荷（非 postcard）必须自描述长度，解析器按 desc_len 截取条目区、
//!   忽略其后的传输填充；
//! - v1 不含参数/返回类型签名（postcard schema 为后续扩展方向）。
//!
//! 尺寸上界：18 方法 × （varint + 1 + varint + 15B 名）≈ 320B，远低于
//! 单条消息 2028B 预算。

/// 描述符协议版本（线格式第 1 字节）。
pub const PROTOCOL_VERSION: u8 = 1;

/// flags bit0：单向（send / urgent）。
pub const FLAG_ONE_WAY: u8 = 1 << 0;
/// flags bit1：急停通道（urgent）。
pub const FLAG_URGENT: u8 = 1 << 1;
/// flags bit2：异步完成（acall，响应由完成方补发）。
pub const FLAG_ACALL: u8 = 1 << 2;

// ── const 编码（宏在编译期调用）──────────────────────────────────────────

const fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 128 {
        v >>= 7;
        n += 1;
    }
    n
}

const fn put_varint(buf: &mut [u8], pos: &mut usize, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf[*pos] = b;
            *pos += 1;
            return;
        }
        buf[*pos] = b | 0x80;
        *pos += 1;
    }
}

/// 计算描述符总长（与 [`encode`] 的写入量严格一致；宏用于定长数组）。
///
/// `flags` 不参与长度（每方法恰 1 字节），参数保留只为与 [`encode`]
/// 同签名成对调用。
pub const fn encoded_len(mids: &[u64], _flags: &[u8], names: &[&str]) -> usize {
    // body = count varint + 条目区；总长 = proto + desc_len varint + body
    let mut body = varint_len(mids.len() as u64);
    let mut i = 0;
    while i < mids.len() {
        body += varint_len(mids[i]) + 1 + varint_len(names[i].len() as u64) + names[i].len();
        i += 1;
    }
    1 + varint_len(body as u64) + body
}

/// 编码进定长数组（`N` 须等于 [`encoded_len`] 的返回值，多写/少写都会
/// 在 const 求值期越界 panic——编译期即错）。
pub const fn encode<const N: usize>(mids: &[u64], flags: &[u8], names: &[&str]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut pos = 0;
    out[pos] = PROTOCOL_VERSION;
    pos += 1;
    // desc_len = body 长度（count varint + 条目区），与 encoded_len 同式
    let mut body = varint_len(mids.len() as u64);
    let mut i = 0;
    while i < mids.len() {
        body += varint_len(mids[i]) + 1 + varint_len(names[i].len() as u64) + names[i].len();
        i += 1;
    }
    put_varint(&mut out, &mut pos, body as u64);
    put_varint(&mut out, &mut pos, mids.len() as u64);
    let mut i = 0;
    while i < mids.len() {
        put_varint(&mut out, &mut pos, mids[i]);
        out[pos] = flags[i];
        pos += 1;
        put_varint(&mut out, &mut pos, names[i].len() as u64);
        let b = names[i].as_bytes();
        let mut j = 0;
        while j < b.len() {
            out[pos] = b[j];
            pos += 1;
            j += 1;
        }
        i += 1;
    }
    out
}

// ── 运行时解析（客户端用，零分配）───────────────────────────────────────

/// 一条方法的描述（借用自描述符字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodDesc<'a> {
    /// 方法 ID（协议 flag 剥离后的实际值）。
    pub mid: u64,
    /// 调用形态标志位（FLAG_* 组合）。
    pub flags: u8,
    /// 方法名（宏侧为常量名 stringify）。
    pub name: &'a str,
}

impl<'a> MethodDesc<'a> {
    /// 单向调用（send / urgent）。
    pub const fn is_one_way(&self) -> bool {
        self.flags & FLAG_ONE_WAY != 0
    }
    /// 急停通道（CH2）。
    pub const fn is_urgent(&self) -> bool {
        self.flags & FLAG_URGENT != 0
    }
    /// 异步完成（响应由服务端完成方补发）。
    pub const fn is_deferred(&self) -> bool {
        self.flags & FLAG_ACALL != 0
    }
    /// 调用形态名（"call" / "send" / "urgent" / "acall"）。
    pub fn kind_name(&self) -> &'static str {
        if self.is_deferred() {
            "acall"
        } else if self.is_urgent() {
            "urgent"
        } else if self.is_one_way() {
            "send"
        } else {
            "call"
        }
    }
}

/// 已解析的服务描述符视图。
#[derive(Debug, Clone, Copy)]
pub struct Descriptor<'a> {
    proto: u8,
    bytes: &'a [u8],
}

/// 方法迭代器（游标式，零分配）。
#[derive(Debug, Clone)]
pub struct Methods<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Methods<'a> {
    type Item = MethodDesc<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut pos = self.pos;
        let mid = take_varint(self.bytes, &mut pos)?;
        let flags = *self.bytes.get(pos)?;
        pos += 1;
        let name_len = take_varint(self.bytes, &mut pos)? as usize;
        let end = pos.checked_add(name_len)?;
        let name = core::str::from_utf8(self.bytes.get(pos..end)?).ok()?;
        self.pos = end;
        Some(MethodDesc { mid, flags, name })
    }
}

fn take_varint(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        // u64 varint 至多 10 组；越界组 = 畸形数据
        if shift >= 64 {
            return None;
        }
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
    }
}

impl<'a> Descriptor<'a> {
    /// 描述符协议版本。
    pub const fn proto(&self) -> u8 {
        self.proto
    }

    /// 方法迭代器。
    pub fn methods(&self) -> Methods<'a> {
        Methods {
            bytes: self.bytes,
            pos: 0,
        }
    }

    /// 方法数（线性扫描；小表无妨）。
    pub fn method_count(&self) -> usize {
        self.methods().count()
    }
}

/// 解析描述符字节。
///
/// 按 `desc_len` 截取条目区；其后的字节是单块传输的零填充（定长 255B
/// 线格式），忽略。返回 `None`：协议版本不认识、desc_len 越界、头/条目
/// 畸形、名称非 UTF-8、条目数与 desc_len 不严格一致——把"服务端发来的
/// 东西解不开"显式化而不是静默半解。
pub fn parse(bytes: &[u8]) -> Option<Descriptor<'_>> {
    let proto = *bytes.first()?;
    if proto != PROTOCOL_VERSION {
        return None;
    }
    let mut pos = 1;
    let desc_len = take_varint(bytes, &mut pos)? as usize;
    let body = bytes.get(pos..pos.checked_add(desc_len)?)?;
    // 全量预校验：恰解析 count 条且游标走满 desc_len（多一条、少一条
    // 都拒；desc_len 之外的填充不校验）
    let mut inner = 0;
    let count = take_varint(body, &mut inner)? as usize;
    let mut m = Methods {
        bytes: body.get(inner..)?,
        pos: 0,
    };
    for _ in 0..count {
        m.next()?;
    }
    if m.pos != body.len() - inner {
        return None;
    }
    Some(Descriptor {
        proto,
        bytes: body.get(inner..)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIDS: [u64; 3] = [1, 2, 18];
    const FLAGS: [u8; 3] = [0, FLAG_ONE_WAY, FLAG_ACALL];
    const NAMES: [&str; 3] = ["ECHO", "DELAY", "ARM_RELEASE"];

    #[test]
    fn const_encode_runtime_parse_roundtrip() {
        const LEN: usize = encoded_len(&MIDS, &FLAGS, &NAMES);
        const ARR: [u8; LEN] = encode::<LEN>(&MIDS, &FLAGS, &NAMES);

        let d = parse(&ARR).expect("const 生成的字节必须可解析");
        assert_eq!(d.proto(), PROTOCOL_VERSION);
        assert_eq!(d.method_count(), 3);
        let ms: Vec<_> = d.methods().collect();
        assert_eq!(ms[0].name, "ECHO");
        assert_eq!(ms[0].mid, 1);
        assert_eq!(ms[0].kind_name(), "call");
        assert_eq!(ms[1].name, "DELAY");
        assert!(ms[1].is_one_way() && !ms[1].is_urgent());
        assert_eq!(ms[2].name, "ARM_RELEASE");
        assert!(ms[2].is_deferred());
        assert_eq!(ms[2].kind_name(), "acall");
    }

    #[test]
    fn varint_boundaries() {
        // 单组 / 多组 / u64 上界的 mid
        const MIDS: [u64; 6] = [0, 127, 128, 16383, 16384, u64::MAX];
        const FLAGS: [u8; 6] = [0; 6];
        const NAMES: [&str; 6] = ["A", "B", "C", "D", "E", "F"];
        const LEN: usize = encoded_len(&MIDS, &FLAGS, &NAMES);
        const ARR: [u8; LEN] = encode::<LEN>(&MIDS, &FLAGS, &NAMES);
        let d = parse(&ARR).unwrap();
        let parsed: Vec<u64> = d.methods().map(|m| m.mid).collect();
        assert_eq!(parsed, MIDS.to_vec());
    }

    #[test]
    fn reject_garbage() {
        assert!(parse(&[]).is_none());
        // 版本不符
        assert!(parse(&[9, 0]).is_none());
        // desc_len 越界
        assert!(parse(&[PROTOCOL_VERSION, 200, 0]).is_none());
        // desc_len 之外的零填充（单块传输语义）：按长度截取，解析成功
        const A_MIDS: [u64; 1] = [1];
        const A_FLAGS: [u8; 1] = [0];
        const A_NAMES: [&str; 1] = ["X"];
        const A_LEN: usize = encoded_len(&A_MIDS, &A_FLAGS, &A_NAMES);
        const A_ARR: [u8; A_LEN] = encode::<A_LEN>(&A_MIDS, &A_FLAGS, &A_NAMES);
        let mut padded = A_ARR.to_vec();
        padded.extend_from_slice(&[0u8; 5]);
        assert!(parse(&padded).is_some(), "desc_len 外的零填充应被忽略");
        // count 大于实际条目数（proto@0、desc_len@1（<128 单字节）、count@2）
        let mut bad = A_ARR;
        bad[2] += 1;
        assert!(parse(&bad).is_none());
        // 名称非 UTF-8
        assert!(parse(&[PROTOCOL_VERSION, 5, 1, 1, 0, 1, 0xFF]).is_none());
    }
}
