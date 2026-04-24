// ─── Paso 7: Time Sync — El device sabe qué hora es ───
//
// Agregamos inicialización SNTP después de conectar a WiFi. Una vez
// sincronizado, get_current_hm() devuelve la hora/minuto reales — base
// para el scheduler de paso-08.
//
// Módulo nuevo: time_sync
// Resto heredado intacto.

mod led;
mod light_state;
mod provisioning;
mod secure_storage;
mod telemetry;
mod time_sync;
mod wifi;
mod ws_client;

use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

#[allow(unused_imports)]
use esp_idf_svc::sys as _;

use log::{error, info, warn};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use led::LedController;
use light_state::{LightState, Mode};
use secure_storage::SecureStorage;
use telemetry::TelemetryReport;
use ws_client::{OutgoingMessage, WsClient};

const BRIGHTNESS_STEPS: &[u8] = &[0, 25, 50, 75, 100, 75, 50, 25];
const LOOP_TICK_MS: u32 = 500;
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(60);

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    info!("paso-07-time-sync");

    if let Err(e) = run() {
        error!("Error fatal: {:?}", e);
        std::thread::sleep(Duration::from_secs(10));
        unsafe {
            esp_idf_svc::sys::esp_restart();
        }
    }
}

fn run() -> anyhow::Result<()> {
    let boot_time = Instant::now();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs_partition = EspDefaultNvsPartition::take()?;

    let led_controller = LedController::new(peripherals.rmt.channel0, peripherals.pins.gpio2)?;
    let led = Arc::new(Mutex::new(led_controller));

    let storage = SecureStorage::new(nvs_partition.clone())?;
    let storage = Arc::new(Mutex::new(storage));

    let is_provisioned = { storage.lock().unwrap().is_provisioned()? };
    if !is_provisioned {
        warn!("Device not provisioned!");
        provisioning::start_provisioning(peripherals.modem, sysloop, storage)?;
        return Ok(());
    }

    let credentials = { storage.lock().unwrap().load_credentials()? };
    let device_id = credentials.device_id.clone();

    info!(
        "Device ID: {} — Connecting to WiFi: {}",
        device_id, credentials.wifi_ssid
    );
    let _wifi = wifi::connect(
        &credentials.wifi_ssid,
        &credentials.wifi_password,
        peripherals.modem,
        sysloop,
    )?;
    info!("WiFi connected!");
    drop(credentials);

    // ─── NUEVO EN PASO 7: inicializar SNTP ───
    //
    // _sntp debe mantenerse vivo durante todo el firmware. Si se dropea,
    // el cliente SNTP para de sincronizarse y el reloj va a driftear
    // (el RTC del ESP32-C3 tiene ±10ppm — ~30s/mes de error sin SNTP).

    let _sntp = time_sync::init_ntp()?;

    let light_state = Arc::new(Mutex::new(LightState::default()));
    let ws = WsClient::new(light_state.clone())?;
    ws.send(OutgoingMessage::Hello {
        device_id: device_id.clone(),
    })?;

    info!("Entering main loop");

    let mut step_idx: usize = 0;
    let mut last_manual_intensity: u8 = 255;
    let mut next_telemetry = Instant::now() + TELEMETRY_INTERVAL;

    loop {
        let snapshot = { *light_state.lock().unwrap() };

        match snapshot.mode {
            Mode::Auto => {
                let step = BRIGHTNESS_STEPS[step_idx];
                led.lock().unwrap().set_brightness(step)?;
                light_state.lock().unwrap().intensity = step;
                step_idx = (step_idx + 1) % BRIGHTNESS_STEPS.len();
                last_manual_intensity = 255;
            }
            Mode::Manual => {
                if snapshot.intensity != last_manual_intensity {
                    led.lock().unwrap().set_brightness(snapshot.intensity)?;
                    last_manual_intensity = snapshot.intensity;
                }
            }
        }

        if Instant::now() >= next_telemetry {
            let mode_str = match snapshot.mode {
                Mode::Auto => "auto",
                Mode::Manual => "manual",
            };

            let report = TelemetryReport::new(boot_time)
                .with_heap()
                .with_light_state(snapshot.intensity, mode_str);

            // Log con la hora del sistema si está disponible
            if let Some(s) = time_sync::get_local_time_string() {
                info!(
                    "[{}] Telemetry: heap={:?} uptime={}s",
                    s, report.heap_free_bytes, report.uptime_secs
                );
            } else {
                info!(
                    "[no-time] Telemetry: heap={:?} uptime={}s",
                    report.heap_free_bytes, report.uptime_secs
                );
            }

            if let Err(e) = ws.send(OutgoingMessage::Telemetry(report)) {
                warn!("Failed to enqueue telemetry: {:?}", e);
            }

            next_telemetry = Instant::now() + TELEMETRY_INTERVAL;
        }

        FreeRtos::delay_ms(LOOP_TICK_MS);
    }
}
