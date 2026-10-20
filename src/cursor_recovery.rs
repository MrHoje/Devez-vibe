use std::{io, time::Duration};

// AppServer declares this after its job handle, so recovery also runs after
// owned helpers are terminated during provider changes, shutdown, or unwinding.
pub struct RestoreOnDrop;

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        let _ = restore_if_idle();
    }
}

pub async fn restore_after_turn() -> io::Result<()> {
    // The helper can release its overlay after turn/completed arrives.
    for delay in [0, 100, 500] {
        if delay != 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if !restore_if_idle()? {
            break;
        }
    }
    Ok(())
}

/// Returns true only when a blank cursor is still owned by a visible helper.
pub fn restore_if_idle() -> io::Result<bool> {
    // Unit tests may inspect private, off-screen cursor bitmaps, but must never
    // reload the desktop cursor scheme as a side effect of app-server tests.
    #[cfg(windows)]
    if !cfg!(test) {
        return windows::restore_if_idle();
    }
    Ok(false)
}

#[cfg(windows)]
mod windows {
    use std::{io, mem::size_of, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HWND, LPARAM},
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetObjectW, SelectObject,
        },
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{
            DI_NORMAL, DrawIconEx, EnumWindows, GetClassNameW, GetIconInfo,
            GetWindowThreadProcessId, HCURSOR, ICONINFO, IDC_ARROW, IDC_HAND, IDC_IBEAM,
            IsWindowVisible, LoadCursorW, SPI_SETCURSORS, SystemParametersInfoW,
        },
    };

    pub(super) fn restore_if_idle() -> io::Result<bool> {
        unsafe {
            let blank = [IDC_ARROW, IDC_IBEAM, IDC_HAND].into_iter().any(|id| {
                let cursor = LoadCursorW(ptr::null_mut(), id);
                !cursor.is_null() && is_blank(cursor)
            });
            if !blank {
                return Ok(false);
            }
            let mut active = false;
            if EnumWindows(
                Some(find_helper_overlay),
                ptr::from_mut(&mut active) as LPARAM,
            ) == 0
                && !active
            {
                return Err(io::Error::last_os_error());
            }
            if active {
                return Ok(true);
            }
            // Reload the user's current scheme; do not replace it with a default
            // arrow or change ShowCursor's per-thread display counter.
            if SystemParametersInfoW(SPI_SETCURSORS, 0, ptr::null_mut(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(false)
    }

    unsafe extern "system" fn find_helper_overlay(window: HWND, state: LPARAM) -> i32 {
        unsafe {
            if IsWindowVisible(window) == 0 {
                return 1;
            }
            let mut class = [0; 256];
            let length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
            let mut helper = String::from_utf16_lossy(&class[..length.max(0) as usize])
                .starts_with("CodexComputerUse");
            if !helper {
                let mut pid = 0;
                GetWindowThreadProcessId(window, &mut pid);
                let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !process.is_null() {
                    let mut path = [0; 32768];
                    let mut length = path.len() as u32;
                    if QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) != 0 {
                        let path = String::from_utf16_lossy(&path[..length as usize]);
                        helper = matches!(
                            path.rsplit('\\')
                                .next()
                                .unwrap_or("")
                                .to_ascii_lowercase()
                                .as_str(),
                            "codex-computer-use.exe"
                                | "codex-computer-use-arm64.exe"
                                | "codex-computer-use-swift.exe"
                        );
                    }
                    CloseHandle(process);
                }
            }
            if helper {
                *(state as *mut bool) = true;
                return 0;
            }
            1
        }
    }

    /// Draw onto black and white off-screen surfaces. CURSOR_SHOWING and a
    /// non-null handle can both describe an entirely transparent cursor.
    fn is_blank(cursor: HCURSOR) -> bool {
        unsafe {
            let mut icon = ICONINFO::default();
            if GetIconInfo(cursor, &mut icon) == 0 {
                return false;
            }
            let mut bitmap = BITMAP::default();
            let monochrome = icon.hbmColor.is_null();
            let source = if monochrome {
                icon.hbmMask
            } else {
                icon.hbmColor
            };
            let read = GetObjectW(
                source,
                size_of::<BITMAP>() as i32,
                ptr::from_mut(&mut bitmap).cast(),
            );
            if !icon.hbmMask.is_null() {
                DeleteObject(icon.hbmMask);
            }
            if !icon.hbmColor.is_null() {
                DeleteObject(icon.hbmColor);
            }
            let width = bitmap.bmWidth;
            let height = if monochrome {
                bitmap.bmHeight / 2
            } else {
                bitmap.bmHeight
            };
            if read == 0 || width <= 0 || height <= 0 || width > 1024 || height > 1024 {
                return false;
            }
            let dc = CreateCompatibleDC(ptr::null_mut());
            if dc.is_null() {
                return false;
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = ptr::null_mut();
            let surface =
                CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
            let mut blank = false;
            if !surface.is_null() && !bits.is_null() {
                let previous = SelectObject(dc, surface);
                if !previous.is_null() && previous as isize != -1 {
                    let count = (width * height) as usize;
                    blank = [0, 0x00ff_ffff].into_iter().all(|background| {
                        std::slice::from_raw_parts_mut(bits.cast::<u32>(), count).fill(background);
                        DrawIconEx(
                            dc,
                            0,
                            0,
                            cursor,
                            width,
                            height,
                            0,
                            ptr::null_mut(),
                            DI_NORMAL,
                        ) != 0
                            && GdiFlush() != 0
                            && std::slice::from_raw_parts(bits.cast::<u32>(), count)
                                .iter()
                                .all(|pixel| pixel & 0x00ff_ffff == background)
                    });
                    SelectObject(dc, previous);
                }
            }
            if !surface.is_null() {
                DeleteObject(surface);
            }
            DeleteDC(dc);
            blank
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::{
            Graphics::Gdi::CreateBitmap,
            UI::WindowsAndMessaging::{CreateCursor, CreateIconIndirect, DestroyCursor},
        };

        #[test]
        fn transparent_and_visible_monochrome_cursors_are_distinguished_off_screen() {
            for (and, xor, blank) in [(255u8, 0u8, true), (0, 0, false), (255, 255, false)] {
                let and = [and; 128];
                let xor = [xor; 128];
                unsafe {
                    let cursor = CreateCursor(
                        ptr::null_mut(),
                        0,
                        0,
                        32,
                        32,
                        and.as_ptr().cast(),
                        xor.as_ptr().cast(),
                    );
                    assert!(!cursor.is_null());
                    let actual = is_blank(cursor);
                    DestroyCursor(cursor);
                    assert_eq!(actual, blank);
                }
            }
        }

        #[test]
        fn transparent_color_and_opaque_black_cursors_are_distinguished_off_screen() {
            for (color, blank) in [(0u32, true), (0xff00_0000, false), (0xffff_ffff, false)] {
                unsafe {
                    let pixels = [color; 1024];
                    let mask = [255u8; 128];
                    let info = ICONINFO {
                        hbmColor: CreateBitmap(32, 32, 1, 32, pixels.as_ptr().cast()),
                        hbmMask: CreateBitmap(32, 32, 1, 1, mask.as_ptr().cast()),
                        ..Default::default()
                    };
                    let cursor = CreateIconIndirect(&info);
                    DeleteObject(info.hbmColor);
                    DeleteObject(info.hbmMask);
                    assert!(!cursor.is_null());
                    let actual = is_blank(cursor);
                    DestroyCursor(cursor);
                    assert_eq!(actual, blank, "color: {color:08x}");
                }
            }
        }

        #[test]
        fn invalid_cursor_is_never_treated_as_transparent() {
            assert!(!is_blank(ptr::null_mut()));
        }
    }
}
