//! 声明式 RPC 服务定义宏

/// 定义一个 RPC 服务
///
/// 自动生成结构体、方法 ID 常量和 [`RpcHandler`](crate::RpcHandler) 实现。
///
/// # 三种方法类型
///
/// - `call` — 请求-响应模式，handler 返回响应
/// - `send` — 单向，handler 不返回响应
/// - `urgent` — 急停，走高优先级通道，不返回响应
///
/// # 示例
///
/// ```ignore
/// ov_rpc::define_service! {
///     pub MotorService {
///         SET_SPEED: 0 => send set_speed(motor: u8, speed: i32);
///         STOP:      1 => urgent stop();
///         GET_SPEED: 2 => call get_speed(motor: u8) -> i32;
///     }
/// }
///
/// impl MotorService {
///     pub fn set_speed(motor: u8, speed: i32) { /* ... */ }
///     pub fn stop() { /* ... */ }
///     pub fn get_speed(motor: u8) -> i32 { /* ... */ }
/// }
/// ```
#[macro_export]
macro_rules! define_service {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident {
            $(
                $const_name:ident : $mid:literal => $kind:ident $method:ident
                    ($($arg:ident : $aty:ty),* $(,)?) $(-> $ret:ty)?;
            )*
        }
    ) => {
        $(#[$meta])*
        $vis struct $name;

        impl $name {
            $(
                #[allow(non_upper_case_globals)]
                pub const $const_name: $crate::MethodId = $mid;
            )*
        }

        impl $crate::RpcHandler for $name {
            fn handle(
                method: $crate::MethodId,
                msg: ov_channels::Message,
            ) -> Result<Option<ov_channels::Message>, $crate::DeserializeFailed> {
                match method {
                    $(
                        $mid => {
                            $crate::__dispatch!(
                                msg, $name :: $method,
                                $kind,
                                ($($aty),*) $(-> $ret)?,
                                ($($arg),*)
                            )
                        }
                    )*
                    _ => Ok(None),
                }
            }
        }
    };
}

/// 内部宏：生成反序列化 + 调用 + 序列化代码
#[macro_export]
#[doc(hidden)]
macro_rules! __dispatch {
    // ── call (request-response) ──

    // 0 args
    ($msg:ident, $name:ident :: $method:ident, call, () -> $ret:ty, ()) => {{
        let (rid, _, _): ($crate::RequestId, $crate::MethodId, ()) = $msg.as_request()
            .ok_or($crate::DeserializeFailed)?;
        let result: $ret = $name::$method();
        Ok(ov_channels::Message::response(rid, &result).ok())
    }};

    // 1 arg
    ($msg:ident, $name:ident :: $method:ident, call, ($a:ty) -> $ret:ty, ($an:ident)) => {{
        let (rid, _, $an): ($crate::RequestId, $crate::MethodId, $a) = $msg.as_request()
            .ok_or($crate::DeserializeFailed)?;
        let result: $ret = $name::$method($an);
        Ok(ov_channels::Message::response(rid, &result).ok())
    }};

    // 2 args
    (
        $msg:ident, $name:ident :: $method:ident, call,
        ($a:ty, $b:ty) -> $ret:ty,
        ($an:ident, $bn:ident)
    ) => {{
        let (rid, _, ($an, $bn)): ($crate::RequestId, $crate::MethodId, ($a, $b)) =
            $msg.as_request().ok_or($crate::DeserializeFailed)?;
        let result: $ret = $name::$method($an, $bn);
        Ok(ov_channels::Message::response(rid, &result).ok())
    }};

    // 3 args
    (
        $msg:ident, $name:ident :: $method:ident, call,
        ($a:ty, $b:ty, $c:ty) -> $ret:ty,
        ($an:ident, $bn:ident, $cn:ident)
    ) => {{
        let (rid, _, ($an, $bn, $cn)): ($crate::RequestId, $crate::MethodId, ($a, $b, $c)) =
            $msg.as_request().ok_or($crate::DeserializeFailed)?;
        let result: $ret = $name::$method($an, $bn, $cn);
        Ok(ov_channels::Message::response(rid, &result).ok())
    }};

    // 4 args
    (
        $msg:ident, $name:ident :: $method:ident, call,
        ($a:ty, $b:ty, $c:ty, $d:ty) -> $ret:ty,
        ($an:ident, $bn:ident, $cn:ident, $dn:ident)
    ) => {{
        let (rid, _, ($an, $bn, $cn, $dn)): (
            $crate::RequestId, $crate::MethodId, ($a, $b, $c, $d),
        ) = $msg.as_request().ok_or($crate::DeserializeFailed)?;
        let result: $ret = $name::$method($an, $bn, $cn, $dn);
        Ok(ov_channels::Message::response(rid, &result).ok())
    }};

    // ── send / urgent (one-way, no response) ──

    // 0 args
    ($msg:ident, $name:ident :: $method:ident, $kind:ident, (), ()) => {{
        let (_, _, _): ($crate::RequestId, $crate::MethodId, ()) = $msg.as_request()
            .ok_or($crate::DeserializeFailed)?;
        $name::$method();
        Ok(None)
    }};

    // 1 arg
    ($msg:ident, $name:ident :: $method:ident, $kind:ident, ($a:ty), ($an:ident)) => {{
        let (_, _, $an): ($crate::RequestId, $crate::MethodId, $a) = $msg.as_request()
            .ok_or($crate::DeserializeFailed)?;
        $name::$method($an);
        Ok(None)
    }};

    // 2 args
    (
        $msg:ident, $name:ident :: $method:ident, $kind:ident,
        ($a:ty, $b:ty),
        ($an:ident, $bn:ident)
    ) => {{
        let (_, _, ($an, $bn)): ($crate::RequestId, $crate::MethodId, ($a, $b)) =
            $msg.as_request().ok_or($crate::DeserializeFailed)?;
        $name::$method($an, $bn);
        Ok(None)
    }};

    // 3 args
    (
        $msg:ident, $name:ident :: $method:ident, $kind:ident,
        ($a:ty, $b:ty, $c:ty),
        ($an:ident, $bn:ident, $cn:ident)
    ) => {{
        let (_, _, ($an, $bn, $cn)): ($crate::RequestId, $crate::MethodId, ($a, $b, $c)) =
            $msg.as_request().ok_or($crate::DeserializeFailed)?;
        $name::$method($an, $bn, $cn);
        Ok(None)
    }};

    // 4 args
    (
        $msg:ident, $name:ident :: $method:ident, $kind:ident,
        ($a:ty, $b:ty, $c:ty, $d:ty),
        ($an:ident, $bn:ident, $cn:ident, $dn:ident)
    ) => {{
        let (_, _, ($an, $bn, $cn, $dn)): (
            $crate::RequestId, $crate::MethodId, ($a, $b, $c, $d),
        ) = $msg.as_request().ok_or($crate::DeserializeFailed)?;
        $name::$method($an, $bn, $cn, $dn);
        Ok(None)
    }};
}
