use axplat::init::InitIf;

struct InitIfImpl;

#[impl_plat_interface]
impl InitIf for InitIfImpl {
    fn init_early(_cpu_id: usize, _mbi: usize) {
        axcpu::init::init_trap();
        crate::console::init_early();
        crate::time::init_early();
    }

    #[cfg(feature = "smp")]
    fn init_early_secondary(_cpu_id: usize) {
        axcpu::init::init_trap();
    }

    fn init_later(_cpu_id: usize, _arg: usize) {
        #[cfg(feature = "irq")]
        crate::irq::init_percpu();
        crate::time::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn init_later_secondary(_cpu_id: usize) {
        #[cfg(feature = "irq")]
        crate::irq::init_percpu();
        crate::time::init_percpu();
    }
}
