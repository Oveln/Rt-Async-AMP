//! 声明式 RPC 服务定义宏

/// 定义一个 RPC 服务
///
/// 自动生成结构体、方法 ID 常量和 [`RpcHandler`](crate::RpcHandler) 实现。
/// 用户只需实现对应的业务逻辑函数。
///
/// # 语法
///
/// ```ignore
/// define_service! {
///     $(#[$attrs])*
///     $vis $ServiceName {
///         $CONST_NAME: $method_id => fn $method_name($arg: $ty, ...) -> $ret_ty;
///         ...
///     }
/// }
/// ```
///
/// - `$CONST_NAME`: 方法 ID 常量名（大写，供客户端引用）
/// - `$method_id`: 数值型方法 ID
/// - `$method_name`: 关联函数名（用户需实现）
/// - 支持 0~4 个参数
///
/// # 示例
///
/// ```ignore
/// ov_rpc::define_service! {
///     pub CalcService {
///         ECHO:  0 => fn echo(val: u32) -> u32;
///         ADD:   1 => fn add(a: i32, b: i32) -> i32;
///         PING:  2 => fn ping() -> u32;
///     }
/// }
///
/// // 用户实现业务逻辑（关联函数，无 &self）
/// impl CalcService {
///     pub fn echo(val: u32) -> u32 { val }
///     pub fn add(a: i32, b: i32) -> i32 { a + b }
///     pub fn ping() -> u32 { 42 }
/// }
///
/// // 服务端
/// server.process_all::<CalcService, _>(|msg| { /* 处理非RPC消息 */ });
///
/// // 客户端
/// let rid = client.call_async(CalcService::ECHO, &42u32)?;
/// let result: u32 = client.wait_response(rid)?;
/// ```
#[macro_export]
macro_rules! define_service {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident {
            $($const_name:ident : $mid:literal => fn $method:ident ($($arg:ident : $aty:ty),* $(,)?) -> $ret:ty );* $(;)?
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
            ) -> Option<ov_channels::Message> {
                match method {
                    $(
                        $mid => {
                            $crate::__dispatch!(
                                msg, $name :: $method,
                                ($($aty),*) -> $ret,
                                ($($arg),*)
                            )
                        }
                    )*
                    _ => None,
                }
            }
        }
    };
}

/// 内部宏：根据参数数量生成反序列化 + 调用 + 序列化代码
#[macro_export]
#[doc(hidden)]
macro_rules! __dispatch {
    // 0 args
    ($msg:ident, $name:ident :: $method:ident, () -> $ret:ty, ()) => {{
        let (rid, _, _): ($crate::RequestId, $crate::MethodId, ()) = $msg.as_request()?;
        let result: $ret = $name::$method();
        ov_channels::Message::response(rid, &result).ok()
    }};

    // 1 arg
    ($msg:ident, $name:ident :: $method:ident, ($a:ty) -> $ret:ty, ($an:ident)) => {{
        let (rid, _, $an): ($crate::RequestId, $crate::MethodId, $a) = $msg.as_request()?;
        let result: $ret = $name::$method($an);
        ov_channels::Message::response(rid, &result).ok()
    }};

    // 2 args
    (
        $msg:ident, $name:ident :: $method:ident,
        ($a:ty, $b:ty) -> $ret:ty,
        ($an:ident, $bn:ident)
    ) => {{
        let (rid, _, ($an, $bn)): ($crate::RequestId, $crate::MethodId, ($a, $b)) =
            $msg.as_request()?;
        let result: $ret = $name::$method($an, $bn);
        ov_channels::Message::response(rid, &result).ok()
    }};

    // 3 args
    (
        $msg:ident, $name:ident :: $method:ident,
        ($a:ty, $b:ty, $c:ty) -> $ret:ty,
        ($an:ident, $bn:ident, $cn:ident)
    ) => {{
        let (rid, _, ($an, $bn, $cn)): ($crate::RequestId, $crate::MethodId, ($a, $b, $c)) =
            $msg.as_request()?;
        let result: $ret = $name::$method($an, $bn, $cn);
        ov_channels::Message::response(rid, &result).ok()
    }};

    // 4 args
    (
        $msg:ident, $name:ident :: $method:ident,
        ($a:ty, $b:ty, $c:ty, $d:ty) -> $ret:ty,
        ($an:ident, $bn:ident, $cn:ident, $dn:ident)
    ) => {{
        let (rid, _, ($an, $bn, $cn, $dn)): (
            $crate::RequestId,
            $crate::MethodId,
            ($a, $b, $c, $d),
        ) = $msg.as_request()?;
        let result: $ret = $name::$method($an, $bn, $cn, $dn);
        ov_channels::Message::response(rid, &result).ok()
    }};
}
