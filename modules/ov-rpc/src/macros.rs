//! 声明式 RPC 服务定义宏
//!
//! - [`define_service!`] — 服务端：生成方法 ID 常量 + `RpcHandler` trait impl
//! - [`define_service_client!`] — 客户端：生成类型安全的客户端 struct

/// 定义一个 RPC 服务（服务端用）
///
/// 自动生成结构体、方法 ID 常量和 [`RpcHandler`](crate::RpcHandler) 实现。
/// 服务端需提供对应的方法实现。
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
/// // 服务端实现
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

/// 定义一个 RPC 服务的客户端接口（客户端用）
///
/// 生成类型安全的客户端 struct，内嵌 `RpcClient`，通过 `Deref`/`DerefMut` 暴露收响应方法。
///
/// # 方法类型
///
/// - `call` 方法生成 `method()` + `method_poll()` 两个变体
///   - `client.echo(val, notify)` → call 模式（服务端回 IPI）
///   - `client.echo_poll(val, notify)` → call_poll 模式（不回 IPI，自行轮询）
/// - `send` 方法 → `client.method(args, notify)` 无返回值
/// - `urgent` 方法 → `client.method(args, notify)` 无返回值
///
/// # 示例
///
/// ```ignore
/// ov_rpc::define_service_client! {
///     pub MotorService {
///         SET_SPEED: 0 => send set_speed(motor: u8, speed: i32);
///         GET_SPEED: 2 => call get_speed(motor: u8) -> i32;
///         STOP:      3 => urgent stop();
///     }
/// }
///
/// let mut client = MotorService::new(shm_addr);
///
/// // call 模式（IPI back）
/// let rid = client.get_speed(1u8, || notify())?;
/// // call_poll 模式（自行轮询）
/// let rid = client.get_speed_poll(1u8, || notify())?;
/// // send
/// client.set_speed(1u8, 100i32, || notify())?;
/// // urgent
/// client.stop(|| notify())?;
///
/// // 收响应（通过 Deref 到 RpcClient）
/// client.poll_responses();
/// let speed: i32 = client.recv_for(rid)?.unwrap();
/// ```
#[macro_export]
macro_rules! define_service_client {
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
        $vis struct $name {
            /// 内嵌的 RPC 客户端。
            pub client: $crate::RpcClient,
        }

        impl core::ops::Deref for $name {
            type Target = $crate::RpcClient;
            #[inline]
            fn deref(&self) -> &Self::Target { &self.client }
        }

        impl core::ops::DerefMut for $name {
            #[inline]
            fn deref_mut(&mut self) -> &mut Self::Target { &mut self.client }
        }

        impl $name {
            $(
                #[allow(non_upper_case_globals)]
                pub const $const_name: $crate::MethodId = $mid;
            )*

            /// 创建类型安全的 RPC 客户端。
            pub fn new(shm_addr: usize) -> Self {
                Self { client: $crate::RpcClient::new(shm_addr) }
            }
        }

        // ── 类型安全的调用方法 ──

        impl $name {
            $(
                $crate::__client_dispatch! {
                    $name, $const_name, $method, $kind,
                    ($($aty),*) $(-> $ret)?,
                    ($($arg),*)
                }
            )*
        }
    };
}

/// 内部宏：为客户端生成类型安全的调用方法
#[macro_export]
#[doc(hidden)]
macro_rules! __client_dispatch {
    // ── call (request-response): 生成 method() + method_poll() ──

    // call, 0 args
    ($name:ident, $const:ident, $method:ident, call, () -> $ret:ty, ()) => {
        pub fn $method<N: FnOnce()>(&self, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
            self.client.call($name::$const, &(), notify)
        }
        $crate::paste! {
            pub fn [<$method _poll>]<N: FnOnce()>(&self, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
                self.client.call_poll($name::$const, &(), notify)
            }
        }
    };
    // call, 1 arg
    ($name:ident, $const:ident, $method:ident, call, ($a:ty) -> $ret:ty, ($an:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
            self.client.call($name::$const, &$an, notify)
        }
        $crate::paste! {
            pub fn [<$method _poll>]<N: FnOnce()>(&self, $an: $a, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
                self.client.call_poll($name::$const, &$an, notify)
            }
        }
    };
    // call, 2 args
    ($name:ident, $const:ident, $method:ident, call, ($a:ty, $b:ty) -> $ret:ty, ($an:ident, $bn:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, $bn: $b, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
            self.client.call($name::$const, &($an, $bn), notify)
        }
        $crate::paste! {
            pub fn [<$method _poll>]<N: FnOnce()>(&self, $an: $a, $bn: $b, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
                self.client.call_poll($name::$const, &($an, $bn), notify)
            }
        }
    };
    // call, 3 args
    ($name:ident, $const:ident, $method:ident, call, ($a:ty, $b:ty, $c:ty) -> $ret:ty, ($an:ident, $bn:ident, $cn:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, $bn: $b, $cn: $c, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
            self.client.call($name::$const, &($an, $bn, $cn), notify)
        }
        $crate::paste! {
            pub fn [<$method _poll>]<N: FnOnce()>(&self, $an: $a, $bn: $b, $cn: $c, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
                self.client.call_poll($name::$const, &($an, $bn, $cn), notify)
            }
        }
    };
    // call, 4 args
    ($name:ident, $const:ident, $method:ident, call, ($a:ty, $b:ty, $c:ty, $d:ty) -> $ret:ty, ($an:ident, $bn:ident, $cn:ident, $dn:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, $bn: $b, $cn: $c, $dn: $d, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
            self.client.call($name::$const, &($an, $bn, $cn, $dn), notify)
        }
        $crate::paste! {
            pub fn [<$method _poll>]<N: FnOnce()>(&self, $an: $a, $bn: $b, $cn: $c, $dn: $d, notify: N) -> Result<$crate::RequestId, $crate::SendError> {
                self.client.call_poll($name::$const, &($an, $bn, $cn, $dn), notify)
            }
        }
    };

    // ── send / urgent (one-way) ──

    // 0 args
    ($name:ident, $const:ident, $method:ident, $kind:ident, (), ()) => {
        pub fn $method<N: FnOnce()>(&self, notify: N) -> Result<(), $crate::SendError> {
            self.client.$kind($name::$const, &(), notify)
        }
    };
    // 1 arg
    ($name:ident, $const:ident, $method:ident, $kind:ident, ($a:ty), ($an:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, notify: N) -> Result<(), $crate::SendError> {
            self.client.$kind($name::$const, &$an, notify)
        }
    };
    // 2 args
    ($name:ident, $const:ident, $method:ident, $kind:ident, ($a:ty, $b:ty), ($an:ident, $bn:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, $bn: $b, notify: N) -> Result<(), $crate::SendError> {
            self.client.$kind($name::$const, &($an, $bn), notify)
        }
    };
    // 3 args
    ($name:ident, $const:ident, $method:ident, $kind:ident, ($a:ty, $b:ty, $c:ty), ($an:ident, $bn:ident, $cn:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, $bn: $b, $cn: $c, notify: N) -> Result<(), $crate::SendError> {
            self.client.$kind($name::$const, &($an, $bn, $cn), notify)
        }
    };
    // 4 args
    ($name:ident, $const:ident, $method:ident, $kind:ident, ($a:ty, $b:ty, $c:ty, $d:ty), ($an:ident, $bn:ident, $cn:ident, $dn:ident)) => {
        pub fn $method<N: FnOnce()>(&self, $an: $a, $bn: $b, $cn: $c, $dn: $d, notify: N) -> Result<(), $crate::SendError> {
            self.client.$kind($name::$const, &($an, $bn, $cn, $dn), notify)
        }
    };
}
