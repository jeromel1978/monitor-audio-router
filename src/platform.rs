use std::collections::HashMap;

use windows::core::{
    IInspectable, IUnknown, Interface, Result as WinResult, GUID, HSTRING, PCWSTR,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, DISPLAY_DEVICEW,
    HMONITOR, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, AudioSessionStateActive,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
};

#[derive(Clone, Debug)]
pub struct Monitor {
    pub id: String,
    pub display_name: String,
    pub friendly_name: String,
    pub is_primary: bool,
}

pub struct AudioSession {
    pub process_id: u32,
    pub process_name: String,
    pub volume: f32,
    pub control: ISimpleAudioVolume,
}

pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub volume: f32,
    pub volume_control: IAudioEndpointVolume,
    pub sessions: Vec<AudioSession>,
}

pub struct Snapshot {
    pub monitors: Vec<Monitor>,
    pub devices: Vec<AudioDevice>,
    pub default_device_id: Option<String>,
    pub window_monitors: HashMap<u32, String>,
    pub warnings: Vec<String>,
}

pub fn snapshot() -> WinResult<Snapshot> {
    let (monitors, monitor_lookup_failed) = enumerate_monitors()?;
    let (window_candidates, window_lookup_failed) = enumerate_window_monitors()?;
    let (process_parents, process_lookup_failed) = enumerate_process_parents();
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let (default_device_id, default_device_warning) = unsafe {
        match enumerator.GetDefaultAudioEndpoint(eRender, windows::Win32::Media::Audio::eMultimedia)
        {
            Ok(device) => match device_id(&device) {
                Ok(id) => (Some(id), None),
                Err(error) => (
                    None,
                    Some(format!("Could not read the default output ID: {error}")),
                ),
            },
            Err(error) => (
                None,
                Some(format!("Could not read the default output: {error}")),
            ),
        }
    };
    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
    let count = unsafe { collection.GetCount()? };
    let mut devices = Vec::new();
    let mut warnings = Vec::new();
    if monitor_lookup_failed {
        warnings.push("Windows could not read information for one or more monitors.".to_string());
    }
    if window_lookup_failed {
        warnings.push(
            "Windows could not determine the monitor for one or more app windows.".to_string(),
        );
    }
    if process_lookup_failed {
        warnings.push(
            "Windows could not read process ancestry; some audio sources may not be matched to their app windows."
                .to_string(),
        );
    }
    if let Some(warning) = default_device_warning {
        warnings.push(warning);
    }

    for index in 0..count {
        let device = match unsafe { collection.Item(index) } {
            Ok(device) => device,
            Err(error) => {
                warnings.push(format!("Could not read audio output {index}: {error}"));
                continue;
            }
        };
        match describe_device(&device) {
            Ok(device) => devices.push(device),
            Err(error) => warnings.push(format!("Could not inspect an audio output: {error}")),
        }
    }

    let monitor_ids_by_display_name: HashMap<String, String> = monitors
        .iter()
        .map(|monitor| (monitor.display_name.to_lowercase(), monitor.id.clone()))
        .collect();
    let mut window_monitors = HashMap::new();
    for process_id in devices
        .iter()
        .flat_map(|device| device.sessions.iter())
        .map(|session| session.process_id)
    {
        if let Some(display_name) =
            monitor_for_process(process_id, &window_candidates, &process_parents)
        {
            if let Some(monitor_id) = monitor_ids_by_display_name.get(&display_name.to_lowercase())
            {
                window_monitors.insert(process_id, monitor_id.clone());
            }
        }
    }

    Ok(Snapshot {
        monitors,
        devices,
        default_device_id,
        window_monitors,
        warnings,
    })
}

unsafe fn device_id(device: &IMMDevice) -> WinResult<String> {
    let raw_id = device.GetId()?;
    let id_result = raw_id.to_string();
    CoTaskMemFree(Some(raw_id.0 as _));
    Ok(id_result?)
}

fn describe_device(device: &IMMDevice) -> WinResult<AudioDevice> {
    unsafe {
        let id = device_id(device)?;
        let properties = device.OpenPropertyStore(STGM_READ)?;
        let name = properties.GetValue(&PKEY_Device_FriendlyName)?.to_string();
        let volume_control: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
        let volume = volume_control.GetMasterVolumeLevelScalar()?;
        let sessions = enumerate_sessions(device)?;

        Ok(AudioDevice {
            id,
            name,
            volume,
            volume_control,
            sessions,
        })
    }
}

fn enumerate_sessions(device: &IMMDevice) -> WinResult<Vec<AudioSession>> {
    unsafe {
        let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
        let enumerator = manager.GetSessionEnumerator()?;
        let count = enumerator.GetCount()?;
        let mut sessions = Vec::new();

        for index in 0..count {
            let control = enumerator.GetSession(index)?;
            if control.GetState()? != AudioSessionStateActive {
                continue;
            }

            let control2: IAudioSessionControl2 = control.cast()?;
            let process_id = control2.GetProcessId()?;
            if process_id == 0 {
                continue;
            }

            let volume: ISimpleAudioVolume = control.cast()?;
            let process_name =
                process_name(process_id).unwrap_or_else(|| format!("Process {process_id}"));
            sessions.push(AudioSession {
                process_id,
                process_name,
                volume: volume.GetMasterVolume()?,
                control: volume,
            });
        }

        Ok(sessions)
    }
}

pub fn set_output_volume(device: &AudioDevice, volume: f32) -> WinResult<()> {
    unsafe {
        device
            .volume_control
            .SetMasterVolumeLevelScalar(volume.clamp(0.0, 1.0), std::ptr::null())
    }
}

pub fn set_session_volume(session: &AudioSession, volume: f32) -> WinResult<()> {
    unsafe {
        session
            .control
            .SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null())
    }
}

pub fn route_process(process_id: u32, endpoint_id: &str) -> WinResult<()> {
    let class_name = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
    let inspectable: IInspectable = unsafe { RoGetActivationFactory(&class_name)? };
    let unknown = inspectable.cast::<IUnknown>()?;
    let mut raw_factory = std::ptr::null_mut();
    unsafe {
        (unknown.vtable().QueryInterface)(
            unknown.as_raw(),
            &AUDIO_POLICY_CONFIG_FACTORY_IID,
            &mut raw_factory,
        )
        .ok()?;
    }
    let factory = unsafe { IUnknown::from_raw(raw_factory) };
    let vtable = unsafe { *(factory.as_raw() as *const *const AudioPolicyConfig21H2Vtbl) };
    let device_path = HSTRING::from(endpoint_interface_path(endpoint_id));
    for role in [eConsole, eMultimedia, eCommunications] {
        let result = unsafe {
            ((*vtable).set_persisted_default_endpoint)(
                factory.as_raw(),
                process_id,
                eRender.0,
                role.0,
                device_path.clone(),
            )
        };
        result.ok()?;
    }
    Ok(())
}

fn endpoint_interface_path(endpoint_id: &str) -> String {
    format!(
        r"\\?\SWD#MMDEVAPI#{}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}",
        endpoint_id
    )
}

#[repr(C)]
struct AudioPolicyConfig21H2Vtbl {
    base: windows::core::IInspectable_Vtbl,
    reserved: [usize; 19],
    set_persisted_default_endpoint: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        u32,
        i32,
        i32,
        HSTRING,
    ) -> windows::core::HRESULT,
    get_persisted_default_endpoint: usize,
    clear_persisted_endpoints: usize,
}

const AUDIO_POLICY_CONFIG_FACTORY_IID: GUID =
    GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);

#[derive(Default)]
struct MonitorEnumeration {
    monitors: Vec<Monitor>,
    lookup_failed: bool,
}

fn enumerate_monitors() -> WinResult<(Vec<Monitor>, bool)> {
    let mut enumeration = MonitorEnumeration::default();
    let succeeded = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(monitor_callback),
            LPARAM(&mut enumeration as *mut _ as isize),
        )
    };
    if !succeeded.as_bool() {
        return Err(windows::core::Error::from_win32());
    }
    Ok((enumeration.monitors, enumeration.lookup_failed))
}

unsafe extern "system" fn monitor_callback(
    monitor: HMONITOR,
    _hdc: windows::Win32::Graphics::Gdi::HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let enumeration = &mut *(data.0 as *mut MonitorEnumeration);
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info.monitorInfo as *mut _ as *mut MONITORINFO).as_bool() {
        let end = info
            .szDevice
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(info.szDevice.len());
        let display_name = String::from_utf16_lossy(&info.szDevice[..end]);
        let is_primary = (info.monitorInfo.dwFlags & 1) != 0;
        let (id, friendly_name) = display_identity(&display_name);
        enumeration.monitors.push(Monitor {
            id,
            display_name,
            friendly_name,
            is_primary,
        });
    } else {
        enumeration.lookup_failed = true;
    }
    true.into()
}

#[derive(Default)]
struct WindowEnumeration {
    monitors: HashMap<u32, (String, bool)>,
    lookup_failed: bool,
}

fn enumerate_window_monitors() -> WinResult<(HashMap<u32, (String, bool)>, bool)> {
    let mut enumeration = WindowEnumeration::default();
    unsafe {
        EnumWindows(
            Some(window_callback),
            LPARAM(&mut enumeration as *mut _ as isize),
        )?;
    }
    Ok((enumeration.monitors, enumeration.lookup_failed))
}

unsafe extern "system" fn window_callback(window: HWND, data: LPARAM) -> BOOL {
    if !IsWindowVisible(window).as_bool() || IsIconic(window).as_bool() {
        return true.into();
    }
    let mut process_id = 0;
    GetWindowThreadProcessId(window, Some(&mut process_id));
    if process_id == 0 {
        return true.into();
    }

    let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info.monitorInfo as *mut _ as *mut MONITORINFO).as_bool() {
        let end = info
            .szDevice
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(info.szDevice.len());
        let display_name = String::from_utf16_lossy(&info.szDevice[..end]);
        let enumeration = &mut *(data.0 as *mut WindowEnumeration);
        let foreground = window == GetForegroundWindow();
        let candidate = enumeration
            .monitors
            .entry(process_id)
            .or_insert((display_name.clone(), foreground));
        if foreground && !candidate.1 {
            *candidate = (display_name, true);
        }
    } else {
        let enumeration = &mut *(data.0 as *mut WindowEnumeration);
        enumeration.lookup_failed = true;
    }
    true.into()
}

fn display_identity(display_name: &str) -> (String, String) {
    unsafe {
        let mut adapter = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let mut index = 0;
        while EnumDisplayDevicesW(None, index, &mut adapter, 0).as_bool() {
            let adapter_name = wide_string(&adapter.DeviceName);
            if adapter_name.eq_ignore_ascii_case(display_name) {
                let mut monitor = DISPLAY_DEVICEW {
                    cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                    ..Default::default()
                };
                let monitor_found = EnumDisplayDevicesW(
                    PCWSTR::from_raw(adapter.DeviceName.as_ptr()),
                    0,
                    &mut monitor,
                    0,
                )
                .as_bool();
                if monitor_found {
                    let id = wide_string(&monitor.DeviceID);
                    let friendly_name = wide_string(&monitor.DeviceString);
                    if !id.is_empty() {
                        return (
                            id,
                            if friendly_name.is_empty() {
                                display_name.to_string()
                            } else {
                                friendly_name
                            },
                        );
                    }
                }
                break;
            }
            index += 1;
            adapter.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        }
    }
    (display_name.to_string(), display_name.to_string())
}

fn wide_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn enumerate_process_parents() -> (HashMap<u32, u32>, bool) {
    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(snapshot) => snapshot,
            Err(_) => return (HashMap::new(), true),
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut parents = HashMap::new();
        let mut result = Process32FirstW(snapshot, &mut entry);
        while result.is_ok() {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            result = Process32NextW(snapshot, &mut entry);
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
        (parents, false)
    }
}

fn monitor_for_process(
    process_id: u32,
    window_monitors: &HashMap<u32, (String, bool)>,
    process_parents: &HashMap<u32, u32>,
) -> Option<String> {
    let mut process_id = process_id;
    for _ in 0..32 {
        if let Some((display_name, _)) = window_monitors.get(&process_id) {
            return Some(display_name.clone());
        }
        let parent = process_parents.get(&process_id).copied()?;
        if parent == process_id || parent == 0 {
            return None;
        }
        process_id = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_id_is_converted_to_windows_audio_interface_path() {
        let endpoint_id = "{0.0.0.00000000}.{6f1c2b7e-8d4a-4c59-9e3b-5a7d21c0f1a3}";
        assert_eq!(
            endpoint_interface_path(endpoint_id),
            r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{6f1c2b7e-8d4a-4c59-9e3b-5a7d21c0f1a3}#{e6327cad-dcec-4949-ae8a-991e976a79d2}"
        );
    }

    #[test]
    fn audio_session_matches_visible_window_in_ancestor_process() {
        let windows = HashMap::from([(200, ("\\\\.\\DISPLAY2".to_string(), true))]);
        let parents = HashMap::from([(100, 200), (200, 1)]);

        assert_eq!(
            monitor_for_process(100, &windows, &parents).as_deref(),
            Some("\\\\.\\DISPLAY2")
        );
    }

    #[test]
    fn missing_process_ancestry_does_not_assign_a_monitor() {
        let windows = HashMap::from([(200, ("\\\\.\\DISPLAY2".to_string(), true))]);
        let parents = HashMap::new();

        assert_eq!(monitor_for_process(100, &windows, &parents), None);
    }
}

fn process_name(process_id: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut name = [0u16; 260];
        let length = GetModuleBaseNameW(handle, None, &mut name);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if length == 0 {
            None
        } else {
            Some(String::from_utf16_lossy(&name[..length as usize]))
        }
    }
}
