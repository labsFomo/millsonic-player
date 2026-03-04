use sysinfo::System;

pub fn get_telemetry() -> serde_json::Value {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_mem = sys.total_memory() as f64 / 1_048_576.0;
    let used_mem = sys.used_memory() as f64 / 1_048_576.0;

    serde_json::json!({
        "cpuUsage": sys.global_cpu_usage(),
        "ramUsage": used_mem,
        "ramTotal": total_mem,
        "diskFree": get_disk_free(),
        "diskTotal": get_disk_total(),
    })
}

fn get_disk_free() -> f64 {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    disks.iter()
        .map(|d| d.available_space() as f64 / 1_073_741_824.0)
        .sum()
}

fn get_disk_total() -> f64 {
    use sysinfo::Disks;
    let disks = Disks::new_with_refreshed_list();
    disks.iter()
        .map(|d| d.total_space() as f64 / 1_073_741_824.0)
        .sum()
}
