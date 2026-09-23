#![cfg_attr(windows, windows_subsystem = "windows")]
mod app;

#[cfg(windows)]
mod window {
    use std::{
        ffi::OsStr,
        fs::OpenOptions,
        io,
        mem::forget,
        os::{windows::ffi::OsStrExt, windows::io::AsRawHandle},
    };
    use windows_sys::Win32::{
        Foundation::FALSE,
        System::Console::{
            ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, CONSOLE_FONT_INFOEX, COORD,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleWindow, GetStdHandle,
            STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleMode,
            SetConsoleOutputCP, SetConsoleTitleW, SetCurrentConsoleFontEx, SetStdHandle,
        },
        UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongPtrW, LWA_ALPHA, SetLayeredWindowAttributes,
            SetWindowLongPtrW, WS_EX_LAYERED,
        },
    };

    pub fn init() -> io::Result<()> {
        unsafe {
            if GetConsoleWindow().is_null() {
                if AttachConsole(ATTACH_PARENT_PROCESS) == 0 && AllocConsole() == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            let output = OpenOptions::new().read(true).write(true).open("CONOUT$")?;
            let input = OpenOptions::new().read(true).write(true).open("CONIN$")?;
            if SetStdHandle(STD_OUTPUT_HANDLE, output.as_raw_handle()) == 0
                || SetStdHandle(STD_ERROR_HANDLE, output.as_raw_handle()) == 0
                || SetStdHandle(STD_INPUT_HANDLE, input.as_raw_handle()) == 0
            {
                return Err(io::Error::last_os_error());
            }
            forget(output);
            forget(input);
            SetConsoleOutputCP(65001);
            SetConsoleCP(65001);
            let console_output = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0;
            if GetConsoleMode(console_output, &mut mode) == 0
                || SetConsoleMode(console_output, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut font: CONSOLE_FONT_INFOEX = std::mem::zeroed();
            font.cbSize = std::mem::size_of::<CONSOLE_FONT_INFOEX>() as u32;
            font.dwFontSize = COORD { X: 0, Y: 16 };
            let face: Vec<u16> = OsStr::new("SimHei").encode_wide().chain(Some(0)).collect();
            font.FaceName[..face.len()].copy_from_slice(&face);
            SetCurrentConsoleFontEx(console_output, FALSE, &font);
            let title: Vec<u16> = OsStr::new("ActWeave")
                .encode_wide()
                .chain(Some(0))
                .collect();
            SetConsoleTitleW(title.as_ptr());
            let window = GetConsoleWindow();
            if !window.is_null() {
                let style = GetWindowLongPtrW(window, GWL_EXSTYLE);
                SetWindowLongPtrW(window, GWL_EXSTYLE, style | WS_EX_LAYERED as isize);
                SetLayeredWindowAttributes(window, 0, 225, LWA_ALPHA);
            }
        }
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    window::init()?;
    let local = tokio::task::LocalSet::new();
    local.run_until(app::run()).await
}
