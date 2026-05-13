// Copyright 2025 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod rockchip;

#[cfg(all(feature = "rockchip-soc", not(feature = "rk3568-clk")))]
pub(crate) use rockchip::{
    RockchipPinCtrl, rk3588_enable_clock, rk3588_enable_power_domain, rk3588_reset_assert,
    rk3588_reset_deassert,
};

/// Set the IOMUX function for a single RK3588 pin identified by (bank, pin_in_bank, mux_fn).
///
/// This is a best-effort helper: if `RockchipPinCtrl` is not yet registered or the
/// feature is disabled the call is silently ignored so that callers don't need to
/// worry about ordering.
#[cfg(all(feature = "rockchip-soc", not(feature = "rk3568-clk")))]
pub fn rk3588_set_pin_mux(bank: u32, pin_in_bank: u32, mux: u32) {
    use rockchip_soc::{Iomux, PinId};

    let id = match PinId::from_bank_pin(bank.into(), pin_in_bank) {
        Some(id) => id,
        None => {
            log::warn!("rk3588_set_pin_mux: invalid pin bank={bank} pin={pin_in_bank}");
            return;
        }
    };
    let mux = Iomux::from_bits_truncate(mux as u8);

    let Some(pinctrl) = rdrive::get_one::<RockchipPinCtrl>() else {
        log::warn!(
            "rk3588_set_pin_mux: RockchipPinCtrl not registered, skipping GPIO{bank}_{pin_in_bank} mux={mux:?}"
        );
        return;
    };
    let mut guard = match pinctrl.lock() {
        Ok(g) => g,
        Err(err) => {
            log::warn!("rk3588_set_pin_mux: PinCtrl lock failed: {err}, skipping GPIO{bank}_{pin_in_bank}");
            return;
        }
    };

    if let Err(e) = guard.set_pin_mux(id, mux) {
        log::warn!(
            "rk3588_set_pin_mux: GPIO{bank}_{pin_in_bank} mux={mux:?} failed: {e:?}"
        );
    }
}

#[cfg(not(all(feature = "rockchip-soc", not(feature = "rk3568-clk"))))]
pub fn rk3588_set_pin_mux(_bank: u32, _pin_in_bank: u32, _mux: u32) {
    // rockchip-soc feature not enabled – nothing to do
}


#[cfg(not(all(feature = "rockchip-soc", not(feature = "rk3568-clk"))))]
pub(crate) fn rk3588_enable_clock(id: u32) -> Result<(), rdrive::probe::OnProbeError> {
    Err(rdrive::probe::OnProbeError::other(alloc::format!(
        "RK3588 clock support is not enabled for clock {id:#x}"
    )))
}

#[cfg(not(all(feature = "rockchip-soc", not(feature = "rk3568-clk"))))]
pub(crate) fn rk3588_enable_power_domain(domain: usize) -> Result<(), alloc::string::String> {
    Err(alloc::format!(
        "RK3588 power-domain support is not enabled for power domain {domain}"
    ))
}

#[cfg(not(all(feature = "rockchip-soc", not(feature = "rk3568-clk"))))]
pub(crate) fn rk3588_reset_deassert(id: u64) -> Result<(), rdrive::probe::OnProbeError> {
    Err(rdrive::probe::OnProbeError::other(alloc::format!(
        "RK3588 reset support is not enabled for reset {id:#x}"
    )))
}
