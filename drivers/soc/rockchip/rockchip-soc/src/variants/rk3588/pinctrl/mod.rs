//! RK3588 Pinctrl 模块
//!
//! 提供引脚复用和引脚配置功能。

use crate::{
    GpioDirection, Mmio, PinConfig, PinId,
    pinctrl::{Iomux, PinCtrlOp, PinctrlResult, gpio::{GpioBank, IomuxReg}},
};

mod pinconf_regs;
mod reg;

use reg::*;

pub struct PinCtrl {
    /// Pinctrl 驱动（引脚功能配置）
    pinctrl: PinctrlReg,

    /// 5 个 GPIO Bank（GPIO 数据操作）
    gpio_banks: [GpioBank; 5],
}

unsafe impl Send for PinCtrl {}

impl PinCtrl {
    /// 创建新的 PinManager
    ///
    /// IOC 和 GPIO 寄存器地址必须有效且在生命周期内保持可访问
    ///
    /// 寄存器地址参考设备树：
    /// - IOC: 0xfd5f0000 (syscon@fd5f0000)
    /// - GPIO0-4: 0xfd8a0000, 0xfec20000, 0xfec30000, 0xfec40000, 0xfec50000
    pub fn new(ioc: Mmio, gpio: &[Mmio]) -> Self {
        if gpio.len() != 5 {
            panic!("RK3588 PinCtrl requires 5 GPIO banks");
        }

        // RK3588 BUS_IOC IOMUX register layout (per u-boot pinctrl-rk3588.c):
        // Each GPIO bank occupies 4 groups × 8 bytes = 0x20 bytes in BUS_IOC.
        // Bank N (N=1..4) starts at offset (N-1)*0x20 within BUS_IOC.
        // GPIO0 pins use PMU1_IOC/PMU2_IOC (handled separately in reg.rs).
        //
        // Group offsets within each bank:  A=+0x00, B=+0x08, C=+0x10, D=+0x18
        //
        // So the final BUS_IOC offset for a pin is:
        //   (bank-1)*0x20 + group_within_bank*0x08 + (pin%8>=4 ? 4 : 0)
        //
        // GpioBank stores the *group_within_bank* offset (0,8,0x10,0x18) in
        // its iomux[].offset array. reg.rs adds BUS_IOC (0x8000) but does NOT
        // add the per-bank base. We fix this by pre-biasing each bank's iomux
        // offsets with its BUS_IOC bank base.
        let iomux_for_bank = |bank_bus_offset: usize| -> [IomuxReg; 4] {
            core::array::from_fn(|i| IomuxReg {
                ty: Iomux::WIDTH_4BIT,
                offset: bank_bus_offset + i * 8,
            })
        };

        Self {
            pinctrl: unsafe { PinctrlReg::new(ioc) },
            gpio_banks: [
                // GPIO0: special-cased in reg.rs (PMU1/PMU2 IOC), use offset=0
                GpioBank::new_with_iomux(gpio[0], [
                    IomuxReg { ty: Iomux::WIDTH_4BIT, offset: 0x00 },
                    IomuxReg { ty: Iomux::WIDTH_4BIT, offset: 0x08 },
                    IomuxReg { ty: Iomux::WIDTH_4BIT, offset: 0x10 },
                    IomuxReg { ty: Iomux::WIDTH_4BIT, offset: 0x18 },
                ]),
                // GPIO1: BUS_IOC base offset = 0x0020 (A=0x0020, B=0x0028, C=0x0030, D=0x0038)
                GpioBank::new_with_iomux(gpio[1], iomux_for_bank(0x0020)),
                // GPIO2: BUS_IOC base offset = 0x0040 (A=0x0040, B=0x0048, C=0x0050, D=0x0058)
                GpioBank::new_with_iomux(gpio[2], iomux_for_bank(0x0040)),
                // GPIO3: BUS_IOC base offset = 0x0060 (A=0x0060, B=0x0068, C=0x0070, D=0x0078)
                GpioBank::new_with_iomux(gpio[3], iomux_for_bank(0x0060)),
                // GPIO4: BUS_IOC base offset = 0x0080 (A=0x0080, B=0x0088, C=0x0090, D=0x0098)
                GpioBank::new_with_iomux(gpio[4], iomux_for_bank(0x0080)),
            ],
        }
    }

    /// 读取 GPIO 引脚值
    ///
    /// 引脚必须已配置为 GPIO 功能。
    ///
    /// # 参数
    ///
    /// * `pin` - 引脚 ID
    ///
    /// # 返回
    ///
    /// 引脚电平状态（true = 高电平，false = 低电平）
    pub fn read_gpio(&self, pin: PinId) -> PinctrlResult<bool> {
        let bank_id = pin.bank().raw() as usize;
        self.gpio_banks[bank_id].read(pin)
    }

    /// 写入 GPIO 引脚值
    ///
    /// 引脚必须已配置为 GPIO 输出功能。
    ///
    /// # 参数
    ///
    /// * `pin` - 引脚 ID
    /// * `value` - 输出值（true = 高电平，false = 低电平）
    pub fn write_gpio(&self, pin: PinId, value: bool) -> PinctrlResult<()> {
        let bank_id = pin.bank().raw() as usize;
        self.gpio_banks[bank_id].write(pin, value)
    }

    fn bank(&self, pin: PinId) -> &GpioBank {
        &self.gpio_banks[pin.bank().raw() as usize]
    }

    fn set_mux(&self, config: &PinConfig) -> PinctrlResult<()> {
        self.bank(config.id).verify_mux(config.id, config.mux)?;
        if self.bank(config.id).iomux_gpio_only(config.id) {
            return Ok(());
        }

        let iomux_reg = self.bank(config.id).iomux[config.id.pin_in_bank() as usize / 8];

        self.pinctrl.set_mux(config.id, config.mux, iomux_reg)?;

        Ok(())
    }

    pub fn set_config(&mut self, config: PinConfig) -> PinctrlResult<()> {
        debug!("set_config: {:?}", config);
        self.set_mux(&config)?;
        self.pinctrl.set_pull(config.id, config.pull)?;

        if let Some(drive) = config.drive {
            self.pinctrl.set_drive(config.id, drive)?;
        }

        Ok(())
    }

    pub fn get_config(&self, pin: PinId) -> PinctrlResult<PinConfig> {
        // 获取 IomuxReg（组内偏移）
        let iomux_reg = self.bank(pin).iomux[pin.pin_in_bank() as usize / 8];

        let function = self.pinctrl.get_mux(pin, iomux_reg)?;

        let pull = self.pinctrl.get_pull(pin)?;

        let drive = self.pinctrl.get_drive(pin)?;

        Ok(PinConfig {
            id: pin,
            mux: function,
            pull,
            drive: Some(drive),
        })
    }

    pub fn gpio_direction(&self, pin: PinId) -> PinctrlResult<GpioDirection> {
        self.bank(pin).get_direction(pin)
    }

    pub fn set_gpio_direction(&self, pin: PinId, direction: GpioDirection) -> PinctrlResult<()> {
        self.bank(pin).set_direction(pin, direction)
    }
}

impl PinCtrlOp for PinCtrl {
    fn set_config(&mut self, config: PinConfig) -> PinctrlResult<()> {
        self.set_config(config)
    }

    fn get_config(&self, pin: PinId) -> PinctrlResult<PinConfig> {
        self.get_config(pin)
    }

    fn gpio_direction(&self, pin: PinId) -> PinctrlResult<GpioDirection> {
        self.gpio_direction(pin)
    }

    fn set_gpio_direction(&self, pin: PinId, direction: GpioDirection) -> PinctrlResult<()> {
        self.set_gpio_direction(pin, direction)
    }

    fn read_gpio(&self, pin: PinId) -> PinctrlResult<bool> {
        self.read_gpio(pin)
    }

    fn write_gpio(&self, pin: PinId, value: bool) -> PinctrlResult<()> {
        self.write_gpio(pin, value)
    }
}
