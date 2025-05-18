use std::{collections::HashMap, fmt::Debug, fs, sync::Mutex};
use std::os::unix::io::{FromRawFd, AsRawFd};
use std::io::Read;

use image::RgbaImage;
use scopeguard::defer;
use serde::Deserialize;
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedFd, Type, Value},
};

use crate::{
    error::{XCapError, XCapResult},
    platform::utils::{get_zbus_portal_request, safe_uri_to_path, wait_zbus_response},
};

use super::utils::{get_zbus_connection, png_to_rgba_image};

fn org_gnome_shell_screencast(
    conn: &Connection,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> XCapResult<RgbaImage> {
    let proxy = Proxy::new(
        conn,
        "org.gnome.Shell.Screencast",
        "/org/gnome/Shell/Screencast",
        "org.gnome.Shell.Screencast",
    )?;

    // Create options for the screencast
    let mut options = HashMap::new();
    options.insert("draw-cursor", Value::from(false));
    options.insert("framerate", Value::from(1u32)); // Single frame
    // Use a pipeline that outputs raw RGBA pixels
    let pipeline = format!(
        "videoconvert ! video/x-raw,format=RGBA ! videoconvert ! appsink name=sink sync=false",
    );
    options.insert("pipeline", Value::from(pipeline.as_str()));
    
    // Start the screencast and get the raw pixel data
    let response: (bool, OwnedFd) = proxy.call("ScreencastArea", &(x, y, width, height, "", options))?;
    
    if !response.0 {
        return Err(XCapError::new("Failed to capture screen area"));
    }

    // Read the raw RGBA pixels from the pipe
    let fd = response.1;
    let mut buffer = vec![0u8; (width * height * 4) as usize];
    let mut file = unsafe { std::fs::File::from_raw_fd(fd.as_raw_fd()) };
    file.read_exact(&mut buffer)?;

    // Create the RgbaImage from raw pixels
    RgbaImage::from_raw(width as u32, height as u32, buffer)
        .ok_or_else(|| XCapError::new("Failed to create image from raw pixels"))
}

#[derive(Deserialize, Type, Debug)]
#[zvariant(signature = "dict")]
pub struct ScreenshotResponse {
    uri: String,
}

/// https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Screenshot.html
fn org_freedesktop_portal_screenshot(
    conn: &Connection,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> XCapResult<RgbaImage> {
    let proxy = Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Screenshot",
    )?;

    let handle_token = rand::random::<u32>().to_string();
    let portal_request = get_zbus_portal_request(conn, &handle_token)?;

    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(&handle_token));
    options.insert("modal", Value::from(true));
    options.insert("interactive", Value::from(false));

    // https://github.com/flatpak/xdg-desktop-portal/blob/main/data/org.freedesktop.portal.Screenshot.xml
    proxy.call_method("Screenshot", &("", options))?;
    let screenshot_response: ScreenshotResponse = wait_zbus_response(&portal_request)?;

    let filename = safe_uri_to_path(&screenshot_response.uri)?;
    defer!({
        let _ = fs::remove_file(&filename);
    });

    let rgba_image = png_to_rgba_image(&filename, x, y, width, height)?;

    Ok(rgba_image)
}

static DBUS_LOCK: Mutex<()> = Mutex::new(());

fn wlroots_screenshot(
    x_coordinate: i32,
    y_coordinate: i32,
    width: i32,
    height: i32,
) -> XCapResult<RgbaImage> {
    let wayshot_connection = libwayshot_xcap::WayshotConnection::new()?;
    let capture_region = libwayshot_xcap::region::LogicalRegion {
        inner: libwayshot_xcap::region::Region {
            position: libwayshot_xcap::region::Position {
                x: x_coordinate,
                y: y_coordinate,
            },
            size: libwayshot_xcap::region::Size {
                width: width as u32,
                height: height as u32,
            },
        },
    };
    let rgba_image = wayshot_connection.screenshot(capture_region, false)?;

    // libwayshot returns image 0.24 RgbaImage
    // we need image 0.25 RgbaImage
    let image = image::RgbaImage::from_raw(
        rgba_image.width(),
        rgba_image.height(),
        rgba_image.to_rgba8().into_vec(),
    )
    .expect("Conversion of PNG -> Raw -> PNG does not fail");

    Ok(image)
}

pub fn wayland_capture(x: i32, y: i32, width: i32, height: i32) -> XCapResult<RgbaImage> {
    let lock = DBUS_LOCK.lock();

    let conn = get_zbus_connection()?;
    let res = org_gnome_shell_screencast(conn, x, y, width, height);

    drop(lock);

    res
}

#[test]
fn screnshot_multithreaded() {
    fn make_screenshots() {
        let monitors = crate::monitor::Monitor::all().unwrap();
        for monitor in monitors {
            monitor.capture_image().unwrap();
        }
    }
    // Try making screenshots in paralel. If this times out, then this means that there is a threading issue.
    const PARALELISM: usize = 10;
    let handles: Vec<_> = (0..PARALELISM)
        .map(|_| {
            std::thread::spawn(|| {
                make_screenshots();
            })
        })
        .collect();
    make_screenshots();
    handles
        .into_iter()
        .for_each(|handle| handle.join().unwrap());
}
