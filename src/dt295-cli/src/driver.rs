//! `dt295 driver`: put the units (PIX-DT295(W), PIX-DT195(W)) on WinUSB without Zadig, or
//! give them back to the vendor driver (the PIX-DT195W has none for Windows).
//!
//! `install` uses the WinUSB driver that ships with Windows (`winusb.inf`, "WinUsb
//! Device", hardware id `USB\MS_COMP_WINUSB`): it is picked from the USBDevice class
//! driver list and installed on the unit, which is what Device Manager does for "Let me
//! pick from a list > Universal Serial Bus devices > WinUsb Device". The driver is the
//! inbox, Microsoft-signed one, so no INF or certificate of our own is needed. A unit of
//! another setup class (the vendor driver's MEDIA) is moved to USBDevice first.
//! `restore` installs the vendor driver (provider Pixela) from the driver store again.
//! Windows prefers the vendor driver for a newly plugged unit, so every new unit needs
//! `install` once.
//!
//! Both change the system and need administrator rights: without them the command runs
//! itself again elevated (UAC) in a new window and waits for it.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::Path;

/// `dt295 driver` subcommands.
#[derive(Debug, clap::Subcommand)]
pub enum DriverCommand {
    /// The connected units and the driver each one uses.
    Status,
    /// Bind the units to the WinUSB driver of Windows (asks for administrator rights).
    Install(Target),
    /// Give the units back to the vendor driver (asks for administrator rights).
    Restore(Target),
}

/// Which units `install` / `restore` change.
#[derive(Debug, clap::Args)]
pub struct Target {
    /// Only the unit with this USB serial number (default: every connected unit).
    #[arg(long, value_name = "SERIAL")]
    pub serial: Option<String>,
    /// Set by the elevated re-run: keep its window open at the end.
    #[arg(long, hide = true)]
    pub elevated: bool,
}

/// The hardware ids of the models: (id, whether the vendor has a Windows driver for it).
const HWIDS: [(&str, bool); 4] = [
    (r"USB\VID_06B8&PID_103B", true),  // PIX-DT295
    (r"USB\VID_06B8&PID_103C", true),  // PIX-DT295W
    (r"USB\VID_06B8&PID_103A", true),  // PIX-DT195 (a Mac model; the Xit driver has it)
    (r"USB\VID_06B8&PID_1040", false), // PIX-DT195W (a Mac model)
];

/// A unit as the driver tools see it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unit {
    instance_id: String,
    serial: String,
    /// The vendor has a Windows driver for its model (`restore` can put it back).
    vendor_driver: bool,
    /// The service of the installed driver: `WinUSB`, `PXMB86M0XUSB` (vendor) or empty.
    service: String,
    inf: String,
    provider: String,
}

impl Unit {
    fn on_winusb(&self) -> bool {
        self.service.eq_ignore_ascii_case("WinUSB")
    }
}

/// The serial number at the end of a device instance id (`USB\VID_..&PID_..\<serial>`).
fn serial_of(instance_id: &str) -> &str {
    instance_id.rsplit('\\').next().unwrap_or(instance_id)
}

/// The units `target` selects.
fn select<'a>(units: &'a [Unit], serial: Option<&str>) -> Vec<&'a Unit> {
    units
        .iter()
        .filter(|u| serial.is_none_or(|s| u.serial.eq_ignore_ascii_case(s)))
        .collect()
}

/// One command-line argument quoted for `CommandLineToArgvW` / the MSVC runtime.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

pub fn run(cmd: &DriverCommand) -> anyhow::Result<()> {
    cfg_select! {
        windows => win::run(cmd),
        _ => {
            let _ = cmd;
            anyhow::bail!("dt295 driver is only on Windows (on Linux, see the udev rule in INSTALL.md)")
        }
    }
}

fn print_status(units: &[Unit]) {
    if units.is_empty() {
        println!("no PIX-DT295(W) / PIX-DT195(W) connected");
        return;
    }
    println!(
        "{:<12}  {:<14}  {:<12}  provider",
        "serial", "driver", "inf"
    );
    for u in units {
        let service = if u.service.is_empty() {
            "-"
        } else {
            &u.service
        };
        let inf = if u.inf.is_empty() { "-" } else { &u.inf };
        println!("{:<12}  {service:<14}  {inf:<12}  {}", u.serial, u.provider);
    }
    if units.iter().any(|u| !u.on_winusb()) {
        println!(
            "WinUSB is what the dt295 tools and the BonDriver need: run `dt295 driver install`."
        );
    }
}

/// The `winusb.inf` of this Windows.
fn winusb_inf(windir: &Path) -> std::path::PathBuf {
    windir.join("INF").join("winusb.inf")
}

#[cfg(windows)]
mod win {
    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};

    use anyhow::{Context, anyhow, bail};
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DI_ENUMSINGLEINF, DI_FLAGSEX_ALLOWEXCLUDEDDRVS, DICS_FLAG_GLOBAL, DIGCF_ALLCLASSES,
        DIGCF_PRESENT, DIREG_DRV, DiInstallDevice, HDEVINFO, SETUP_DI_DRIVER_TYPE,
        SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, SP_DEVINSTALL_PARAMS_W, SP_DRVINFO_DATA_V2_W,
        SP_DRVINFO_DETAIL_DATA_W, SPDIT_CLASSDRIVER, SPDIT_COMPATDRIVER, SPDRP_CLASSGUID,
        SPDRP_HARDWAREID, SPDRP_SERVICE, SetupDiBuildDriverInfoList, SetupDiCreateDeviceInfoList,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiEnumDriverInfoW,
        SetupDiGetClassDevsW, SetupDiGetDeviceInstallParamsW, SetupDiGetDeviceInstanceIdW,
        SetupDiGetDeviceRegistryPropertyW, SetupDiGetDriverInfoDetailW, SetupDiOpenDevRegKey,
        SetupDiOpenDeviceInfoW, SetupDiSetDeviceInstallParamsW, SetupDiSetDeviceRegistryPropertyW,
        SetupDiSetSelectedDriverW,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Registry::{KEY_READ, RegCloseKey, RegQueryValueExW};
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::core::GUID;

    use super::{
        DriverCommand, HWIDS, Target, Unit, print_status, quote_arg, select, serial_of, winusb_inf,
    };

    /// GUID_DEVCLASS_USBDEVICE, the setup class of WinUSB devices.
    const CLASS_USBDEVICE: GUID = GUID::from_u128(0x88bae032_5a81_49f0_bc3d_a4ff138216d6);
    const CLASS_USBDEVICE_STR: &str = "{88BAE032-5A81-49F0-BC3D-A4FF138216D6}";
    /// The model of winusb.inf that installs plain WinUSB ("WinUsb Device").
    const WINUSB_MODEL: &str = r"USB\MS_COMP_WINUSB";
    const VENDOR_PROVIDER: &str = "Pixela";
    const SW_SHOWNORMAL: i32 = 1;

    pub fn run(cmd: &DriverCommand) -> anyhow::Result<()> {
        match cmd {
            DriverCommand::Status => {
                print_status(&units()?);
                Ok(())
            }
            DriverCommand::Install(t) => elevated(t, || change(t, Change::Install)),
            DriverCommand::Restore(t) => elevated(t, || change(t, Change::Restore)),
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Change {
        Install,
        Restore,
    }

    /// Run `work` with administrator rights: directly if this process has them, else in an
    /// elevated copy of this command (UAC) whose exit code is passed on.
    fn elevated(t: &Target, work: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
        if is_elevated() {
            let result = work();
            if t.elevated {
                if let Err(e) = &result {
                    println!("error: {e:#}");
                }
                println!("\nPress Enter to close this window.");
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
            }
            return result;
        }
        println!("Administrator rights are needed: continuing in a new window (UAC).");
        match relaunch_elevated()? {
            0 => Ok(()),
            code => bail!("the elevated run ended with exit code {code}"),
        }
    }

    fn change(t: &Target, what: Change) -> anyhow::Result<()> {
        let all = units()?;
        let picked = select(&all, t.serial.as_deref());
        if picked.is_empty() {
            match &t.serial {
                Some(s) => {
                    bail!("no connected PIX-DT295(W) / PIX-DT195(W) with serial {s}")
                }
                None => bail!("no PIX-DT295(W) / PIX-DT195(W) connected"),
            }
        }
        let mut failed = 0;
        for u in picked {
            let r = match what {
                Change::Install if u.on_winusb() => {
                    println!("{}: already on WinUSB", u.serial);
                    continue;
                }
                Change::Restore if u.provider.eq_ignore_ascii_case(VENDOR_PROVIDER) => {
                    println!("{}: already on the vendor driver", u.serial);
                    continue;
                }
                Change::Restore if !u.vendor_driver => {
                    println!("{}: its model has no vendor driver for Windows", u.serial);
                    continue;
                }
                Change::Install => install_winusb(&u.instance_id),
                Change::Restore => install_vendor(&u.instance_id),
            };
            match r {
                Ok(reboot) => println!(
                    "{}: {}{}",
                    u.serial,
                    if what == Change::Install {
                        "WinUSB installed"
                    } else {
                        "vendor driver installed"
                    },
                    if reboot {
                        " (restart Windows to finish)"
                    } else {
                        ""
                    }
                ),
                Err(e) => {
                    println!("{}: failed: {e:#}", u.serial);
                    failed += 1;
                }
            }
        }
        println!();
        print_status(&units()?);
        if failed > 0 {
            bail!("{failed} unit(s) failed");
        }
        Ok(())
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain([0]).collect()
    }

    /// A NUL-terminated wide string from the start of `buf`.
    fn from_wide(buf: &[u16]) -> String {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        OsString::from_wide(&buf[..n])
            .to_string_lossy()
            .into_owned()
    }

    /// The strings of a REG_MULTI_SZ.
    fn multi_sz(buf: &[u16]) -> Vec<String> {
        buf.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(|s| OsString::from_wide(s).to_string_lossy().into_owned())
            .collect()
    }

    fn last_error(what: &str) -> anyhow::Error {
        // SAFETY: no arguments; reads the calling thread's last-error value.
        let code = unsafe { GetLastError() };
        anyhow!(
            "{what}: {} (0x{code:08X})",
            std::io::Error::from_raw_os_error(code as i32)
        )
    }

    fn guid_eq(a: &GUID, b: &GUID) -> bool {
        (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
    }

    /// A device information set; destroyed (with its driver lists) on drop.
    struct DevSet(HDEVINFO);

    impl Drop for DevSet {
        fn drop(&mut self) {
            // SAFETY: a valid set from SetupDiGetClassDevsW / SetupDiCreateDeviceInfoList.
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }

    fn new_devinfo() -> SP_DEVINFO_DATA {
        SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        }
    }

    /// A registry property of a device as raw UTF-16 (`None` if it has none).
    fn property(
        set: &DevSet,
        dev: &SP_DEVINFO_DATA,
        prop: SETUP_DI_REGISTRY_PROPERTY,
    ) -> Option<Vec<u16>> {
        let mut need = 0u32;
        // SAFETY: a size query (no buffer); `need` receives the size in bytes.
        unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                dev,
                prop,
                null_mut(),
                null_mut(),
                0,
                &mut need,
            )
        };
        if need == 0 {
            return None;
        }
        let mut buf = vec![0u16; (need as usize).div_ceil(2) + 1];
        // SAFETY: `buf` holds `need` bytes.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                dev,
                prop,
                null_mut(),
                buf.as_mut_ptr().cast(),
                need,
                null_mut(),
            )
        };
        (ok != 0).then_some(buf)
    }

    /// A string value of the device's driver key (`InfPath`, `ProviderName`).
    fn driver_value(set: &DevSet, dev: &SP_DEVINFO_DATA, name: &str) -> String {
        // SAFETY: a valid set and element; KEY_READ on the driver (software) key.
        let key =
            unsafe { SetupDiOpenDevRegKey(set.0, dev, DICS_FLAG_GLOBAL, 0, DIREG_DRV, KEY_READ) };
        if std::ptr::eq(key, INVALID_HANDLE_VALUE) {
            return String::new();
        }
        let name = wide(name);
        let mut buf = [0u16; 512];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `len` bytes; the key is open.
        let rc = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                null(),
                null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        // SAFETY: the key opened above.
        unsafe { RegCloseKey(key) };
        if rc == 0 {
            from_wide(&buf)
        } else {
            String::new()
        }
    }

    /// The connected units.
    fn units() -> anyhow::Result<Vec<Unit>> {
        let usb = wide("USB");
        // SAFETY: all present devices of the USB enumerator, any class.
        let h = unsafe {
            SetupDiGetClassDevsW(
                null(),
                usb.as_ptr(),
                null_mut(),
                DIGCF_ALLCLASSES | DIGCF_PRESENT,
            )
        };
        if h == INVALID_HANDLE_VALUE as HDEVINFO {
            return Err(last_error("SetupDiGetClassDevs"));
        }
        let set = DevSet(h);
        let mut out = Vec::new();
        for i in 0.. {
            let mut dev = new_devinfo();
            // SAFETY: a valid set; `dev` has its cbSize.
            if unsafe { SetupDiEnumDeviceInfo(set.0, i, &mut dev) } == 0 {
                break;
            }
            let ids = property(&set, &dev, SPDRP_HARDWAREID)
                .map(|b| multi_sz(&b))
                .unwrap_or_default();
            let Some(&(_, vendor_driver)) = HWIDS
                .iter()
                .find(|(hwid, _)| ids.iter().any(|id| id.eq_ignore_ascii_case(hwid)))
            else {
                continue;
            };
            let mut id = [0u16; 512];
            // SAFETY: `id` holds 512 characters.
            if unsafe {
                SetupDiGetDeviceInstanceIdW(
                    set.0,
                    &dev,
                    id.as_mut_ptr(),
                    id.len() as u32,
                    null_mut(),
                )
            } == 0
            {
                continue;
            }
            let instance_id = from_wide(&id);
            out.push(Unit {
                serial: serial_of(&instance_id).to_owned(),
                vendor_driver,
                service: property(&set, &dev, SPDRP_SERVICE)
                    .map(|b| from_wide(&b))
                    .unwrap_or_default(),
                inf: driver_value(&set, &dev, "InfPath"),
                provider: driver_value(&set, &dev, "ProviderName"),
                instance_id,
            });
        }
        out.sort_by(|a, b| a.serial.cmp(&b.serial));
        Ok(out)
    }

    /// A set holding just the device `instance_id`.
    fn open(instance_id: &str) -> anyhow::Result<(DevSet, SP_DEVINFO_DATA)> {
        // SAFETY: an empty set with no class.
        let h = unsafe { SetupDiCreateDeviceInfoList(null(), null_mut()) };
        if h == INVALID_HANDLE_VALUE as HDEVINFO {
            return Err(last_error("SetupDiCreateDeviceInfoList"));
        }
        let set = DevSet(h);
        let mut dev = new_devinfo();
        let id = wide(instance_id);
        // SAFETY: a valid set and a NUL-terminated instance id.
        if unsafe { SetupDiOpenDeviceInfoW(set.0, id.as_ptr(), null_mut(), 0, &mut dev) } == 0 {
            return Err(last_error("SetupDiOpenDeviceInfo"));
        }
        Ok((set, dev))
    }

    /// One driver node of a built driver list, with its first hardware id.
    struct Driver {
        data: SP_DRVINFO_DATA_V2_W,
        hwid: String,
        provider: String,
    }

    fn drivers(set: &DevSet, dev: &SP_DEVINFO_DATA, kind: SETUP_DI_DRIVER_TYPE) -> Vec<Driver> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut data = SP_DRVINFO_DATA_V2_W {
                cbSize: size_of::<SP_DRVINFO_DATA_V2_W>() as u32,
                ..Default::default()
            };
            // SAFETY: a built driver list of this kind; `data` has its cbSize.
            if unsafe { SetupDiEnumDriverInfoW(set.0, dev, kind, i, &mut data) } == 0 {
                break;
            }
            out.push(Driver {
                hwid: detail_hwid(set, dev, &data).unwrap_or_default(),
                // copied out (here and below): the SetupAPI structs are byte-packed
                // on x86, so their fields cannot be borrowed
                provider: from_wide(&{ data.ProviderName }),
                data,
            });
        }
        out
    }

    /// The first hardware id of a driver node (from its detail data).
    fn detail_hwid(
        set: &DevSet,
        dev: &SP_DEVINFO_DATA,
        data: &SP_DRVINFO_DATA_V2_W,
    ) -> Option<String> {
        let mut need = 0u32;
        // SAFETY: a size query (no buffer).
        unsafe { SetupDiGetDriverInfoDetailW(set.0, dev, data, null_mut(), 0, &mut need) };
        let fixed = size_of::<SP_DRVINFO_DETAIL_DATA_W>();
        let size = (need as usize).max(fixed);
        // u64 storage keeps the structure aligned
        let mut buf = vec![0u64; size.div_ceil(8)];
        let detail = buf.as_mut_ptr().cast::<SP_DRVINFO_DETAIL_DATA_W>();
        // SAFETY: `buf` is at least the fixed size and aligned for the structure.
        unsafe { (*detail).cbSize = fixed as u32 };
        // SAFETY: `buf` holds `size` bytes.
        if unsafe { SetupDiGetDriverInfoDetailW(set.0, dev, data, detail, size as u32, null_mut()) }
            == 0
        {
            return None;
        }
        let offset = std::mem::offset_of!(SP_DRVINFO_DETAIL_DATA_W, HardwareID);
        let chars = (size - offset) / 2;
        // SAFETY: the hardware id multi-string runs from HardwareID to the end of `buf`.
        let ids = unsafe {
            std::slice::from_raw_parts(detail.cast::<u8>().add(offset).cast::<u16>(), chars)
        };
        Some(from_wide(ids))
    }

    /// Install `drv` (from a driver list of `set`) on the device.
    fn install(set: &DevSet, dev: &mut SP_DEVINFO_DATA, drv: &mut Driver) -> anyhow::Result<bool> {
        // SAFETY: `drv` is a node of a driver list built for this element.
        if unsafe { SetupDiSetSelectedDriverW(set.0, dev, &mut drv.data) } == 0 {
            return Err(last_error("SetupDiSetSelectedDriver"));
        }
        let mut reboot = 0;
        // SAFETY: as above; no parent window, default flags.
        if unsafe { DiInstallDevice(null_mut(), set.0, dev, &drv.data, 0, &mut reboot) } == 0 {
            return Err(last_error("DiInstallDevice"));
        }
        Ok(reboot != 0)
    }

    fn install_winusb(instance_id: &str) -> anyhow::Result<bool> {
        let (mut set, mut dev) = open(instance_id)?;
        if !guid_eq(&{ dev.ClassGuid }, &CLASS_USBDEVICE) {
            let class = wide(CLASS_USBDEVICE_STR);
            // SAFETY: a REG_SZ (with its NUL) for SPDRP_CLASSGUID.
            let ok = unsafe {
                SetupDiSetDeviceRegistryPropertyW(
                    set.0,
                    &mut dev,
                    SPDRP_CLASSGUID,
                    class.as_ptr().cast(),
                    (class.len() * 2) as u32,
                )
            };
            if ok == 0 {
                return Err(last_error("setting the device class to USBDevice"));
            }
            drop(set);
            (set, dev) = open(instance_id)?;
        }
        let mut params = SP_DEVINSTALL_PARAMS_W {
            cbSize: size_of::<SP_DEVINSTALL_PARAMS_W>() as u32,
            ..Default::default()
        };
        // SAFETY: `params` has its cbSize.
        if unsafe { SetupDiGetDeviceInstallParamsW(set.0, &dev, &mut params) } == 0 {
            return Err(last_error("SetupDiGetDeviceInstallParams"));
        }
        let inf = winusb_inf(&windows_dir());
        let path: Vec<u16> = inf.as_os_str().encode_wide().collect();
        let mut driver_path = params.DriverPath;
        if path.len() >= driver_path.len() {
            bail!("path too long: {}", inf.display());
        }
        driver_path[..path.len()].copy_from_slice(&path);
        driver_path[path.len()] = 0;
        params.DriverPath = driver_path;
        params.Flags |= DI_ENUMSINGLEINF;
        params.FlagsEx |= DI_FLAGSEX_ALLOWEXCLUDEDDRVS;
        // SAFETY: as above.
        if unsafe { SetupDiSetDeviceInstallParamsW(set.0, &dev, &params) } == 0 {
            return Err(last_error("SetupDiSetDeviceInstallParams"));
        }
        // SAFETY: the class list of winusb.inf for this (USBDevice) element.
        if unsafe { SetupDiBuildDriverInfoList(set.0, &mut dev, SPDIT_CLASSDRIVER) } == 0 {
            return Err(last_error("SetupDiBuildDriverInfoList"));
        }
        let mut list = drivers(&set, &dev, SPDIT_CLASSDRIVER);
        let drv = list
            .iter_mut()
            .find(|d| d.hwid.eq_ignore_ascii_case(WINUSB_MODEL))
            .ok_or_else(|| anyhow!("no \"WinUsb Device\" in {}", inf.display()))?;
        install(&set, &mut dev, drv)
    }

    fn install_vendor(instance_id: &str) -> anyhow::Result<bool> {
        let (set, mut dev) = open(instance_id)?;
        // SAFETY: the compatible-driver list from the driver store.
        if unsafe { SetupDiBuildDriverInfoList(set.0, &mut dev, SPDIT_COMPATDRIVER) } == 0 {
            return Err(last_error("SetupDiBuildDriverInfoList"));
        }
        let mut list = drivers(&set, &dev, SPDIT_COMPATDRIVER);
        let drv = list
            .iter_mut()
            .filter(|d| d.provider.eq_ignore_ascii_case(VENDOR_PROVIDER))
            .max_by_key(|d| d.data.DriverVersion)
            .ok_or_else(|| {
                anyhow!(
                    "the vendor driver (provider {VENDOR_PROVIDER}) is not in this PC's driver \
                     store: install the vendor driver package first"
                )
            })?;
        install(&set, &mut dev, drv)
    }

    fn windows_dir() -> PathBuf {
        let mut buf = [0u16; 260];
        // SAFETY: `buf` holds 260 characters.
        let n = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 || n >= buf.len() {
            return PathBuf::from(r"C:\Windows");
        }
        PathBuf::from(OsString::from_wide(&buf[..n]))
    }

    fn is_elevated() -> bool {
        let mut token: HANDLE = null_mut();
        // SAFETY: the current-process pseudo handle; `token` receives a handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        // SAFETY: `e` is a TOKEN_ELEVATION of the given size.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&raw mut e).cast::<c_void>(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
        };
        // SAFETY: the token opened above.
        unsafe { CloseHandle(token) };
        ok != 0 && e.TokenIsElevated != 0
    }

    /// Run this command again elevated (UAC) with `--elevated`, wait, return its exit code.
    fn relaunch_elevated() -> anyhow::Result<u32> {
        let exe = std::env::current_exe().context("the path of dt295.exe")?;
        let args: Vec<String> = std::env::args()
            .skip(1)
            .chain(["--elevated".to_owned()])
            .collect();
        let params = wide(
            &args
                .iter()
                .map(|a| quote_arg(a))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let file: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        let verb = wide("runas");
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: verb.as_ptr(),
            lpFile: file.as_ptr(),
            lpParameters: params.as_ptr(),
            nShow: SW_SHOWNORMAL,
            ..Default::default()
        };
        // SAFETY: `info` and the strings it points to live across the call.
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            // SAFETY: reads the calling thread's last-error value.
            if unsafe { GetLastError() } == ERROR_CANCELLED {
                bail!("administrator rights were declined");
            }
            return Err(last_error("starting the elevated run"));
        }
        let mut code = 1u32;
        // SAFETY: SEE_MASK_NOCLOSEPROCESS gave us the process handle; closed below.
        unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
        }
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(serial: &str, service: &str) -> Unit {
        Unit {
            instance_id: format!(r"{}\{serial}", HWIDS[0].0),
            serial: serial.to_owned(),
            vendor_driver: true,
            service: service.to_owned(),
            inf: String::new(),
            provider: String::new(),
        }
    }

    #[test]
    fn serial_from_instance_id() {
        assert_eq!(serial_of(r"USB\VID_06B8&PID_103B\CD23456789"), "CD23456789");
        assert_eq!(serial_of("plain"), "plain");
    }

    #[test]
    fn selection() {
        let all = [
            unit("CD23456789", "WinUSB"),
            unit("AB12345678", "PXMB86M0XUSB"),
        ];
        assert_eq!(select(&all, None).len(), 2);
        assert_eq!(select(&all, Some("ab12345678"))[0].serial, "AB12345678");
        assert!(select(&all, Some("nope")).is_empty());
        assert!(all[0].on_winusb() && !all[1].on_winusb());
    }

    #[test]
    fn argument_quoting() {
        assert_eq!(quote_arg("install"), "install");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }

    #[test]
    fn inbox_inf_path() {
        assert_eq!(
            winusb_inf(Path::new(r"C:\Windows")),
            Path::new(r"C:\Windows").join("INF").join("winusb.inf")
        );
    }
}
