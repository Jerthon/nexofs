//! T6-06 (Fase 6, "perfis de rede/pausa em bateria") — leitura direta de
//! `/sys/class/power_supply` em vez de D-Bus/UPower: nenhuma dependência
//! nova, funciona sob qualquer desktop environment (GNOME/KDE/nenhum),
//! mesma filosofia de "consultar o estado real do sistema" já usada por
//! `nexofs-content-cache::disk_pressure_level` (`statvfs` real em vez de
//! estimativa própria). Ausência de bateria (desktop) é `false`, não erro —
//! é o comportamento correto ("nunca pausar"), não uma falha a propagar.

use std::path::Path;

const POWER_SUPPLY_DIR: &str = "/sys/class/power_supply";

/// `true` quando pelo menos uma bateria do sistema está descarregando —
/// várias baterias (ex.: laptop com bateria removível + UPS) contam como
/// "em bateria" se qualquer uma estiver descarregando, não só a primeira.
pub fn on_battery() -> bool {
    on_battery_in(Path::new(POWER_SUPPLY_DIR))
}

fn on_battery_in(power_supply_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(power_supply_dir) else {
        return false;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_battery = std::fs::read_to_string(path.join("type")).map(|t| t.trim() == "Battery").unwrap_or(false);
        if !is_battery {
            continue;
        }
        if let Ok(status) = std::fs::read_to_string(path.join("status")) {
            if status.trim() == "Discharging" {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_battery(dir: &Path, name: &str, status: &str) {
        let battery_dir = dir.join(name);
        fs::create_dir_all(&battery_dir).unwrap();
        fs::write(battery_dir.join("type"), "Battery\n").unwrap();
        fs::write(battery_dir.join("status"), format!("{status}\n")).unwrap();
    }

    #[test]
    fn a_machine_with_no_power_supply_directory_is_never_on_battery() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!on_battery_in(&dir.path().join("does-not-exist")));
    }

    #[test]
    fn a_desktop_with_only_an_ac_adapter_is_not_on_battery() {
        let dir = tempfile::tempdir().unwrap();
        let ac_dir = dir.path().join("AC");
        fs::create_dir_all(&ac_dir).unwrap();
        fs::write(ac_dir.join("type"), "Mains\n").unwrap();
        assert!(!on_battery_in(dir.path()));
    }

    #[test]
    fn a_charging_laptop_is_not_on_battery() {
        let dir = tempfile::tempdir().unwrap();
        write_battery(dir.path(), "BAT0", "Charging");
        assert!(!on_battery_in(dir.path()));
    }

    #[test]
    fn a_discharging_laptop_is_on_battery() {
        let dir = tempfile::tempdir().unwrap();
        write_battery(dir.path(), "BAT0", "Discharging");
        assert!(on_battery_in(dir.path()));
    }

    #[test]
    fn any_discharging_battery_among_several_counts() {
        let dir = tempfile::tempdir().unwrap();
        write_battery(dir.path(), "BAT0", "Full");
        write_battery(dir.path(), "BAT1", "Discharging");
        assert!(on_battery_in(dir.path()));
    }
}
