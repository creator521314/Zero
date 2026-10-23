//! 月度用量记账：`agent.root/budget.json`。
//!
//! 存在的理由：`ProviderRouter` 原先把月度用量放在 `Arc<Mutex<f64>>` 里，
//! 注释写「可持久化」但没人落盘——**进程重启即清零**，熔断等于摆设。此处把它
//! 变成真正跨重启的账本：按月分账（跨月自动重置），每次调用后落盘。
//!
//! 两条不容妥协的性质：
//! - **启动绝不被它卡住**：文件损坏/读不了 → 告警并从 0 起记，不能崩在加载阶段；
//! - **落盘失败绝不能打断任务**：磁盘满/权限问题是环境问题，为了记一笔账掀翻
//!   正在跑的任务得不偿失——只 warn。
//!
//! 之所以用 `std::sync::Mutex` 而不是 tokio 的：记账不跨 await，
//! 同步 API 让调用方（包括同步回调）可以直接写，不必 `tokio::spawn`，
//! 也就不会在进程退出时丢掉最后一次计入。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 记账文件名（落在 `agent.root` 下，与 logs/ 同级）。
pub const SPEND_FILE: &str = "budget.json";

/// 保存最近几个月的账目，其余丢弃（防文件无界增长）。
const KEEP_MONTHS: usize = 6;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    /// 月份 → 累计金额（USD）。键形如 `2026-10`。
    #[serde(default)]
    months: BTreeMap<String, f64>,
}

/// 跨重启的月度用量账本。
pub struct SpendStore {
    path: Option<PathBuf>,
    ledger: Mutex<Ledger>,
}

impl SpendStore {
    /// 从文件加载；读不到或读不懂一律回到空账本（并 warn）。
    ///
    /// 一期引入该功能时用户磁盘上并没有这个文件，所以「不存在」是常态，只在
    /// **存在却损坏**时才告警——否则每次启动都会刷一条噪音。
    pub fn load(path: PathBuf) -> Self {
        let ledger = match read_ledger(&path) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "月度用量文件无法读取，本月从 0 重新累计");
                Ledger::default()
            }
        };
        Self {
            path: Some(path),
            ledger: Mutex::new(ledger),
        }
    }

    /// 仅内存：给测试与「不想落盘」的场景用。
    pub fn memory_only() -> Self {
        Self {
            path: None,
            ledger: Mutex::new(Ledger::default()),
        }
    }

    /// 记一笔钱（USD）。返回记完之后本月的累计值。
    pub fn add(&self, cost: f64) -> f64 {
        if !cost.is_finite() || cost <= 0.0 {
            return self.month_total();
        }
        let key = current_month();
        let total = {
            let mut guard = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = guard.months.entry(key).or_insert(0.0);
            *entry += cost;
            let total = *entry;
            prune(&mut guard.months);
            total
        };
        self.flush();
        total
    }

    /// 本月累计（USD）。
    pub fn month_total(&self) -> f64 {
        let key = current_month();
        self.ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .months
            .get(&key)
            .copied()
            .unwrap_or(0.0)
    }

    /// 写回磁盘。失败只 warn——记账失败不该掀翻正在跑的任务。
    ///
    /// 走「临时文件 + rename」：进程在写一半被打断时，留下的仍是上一版的完整文件，
    /// 不会出现半截 JSON 把下次启动搞崩（损坏时我们也会 warn 后从 0 起，但能不碎就别碎）。
    fn flush(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let snapshot = {
            let guard = self
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            serde_json::to_string(&*guard)
        };
        let Ok(text) = snapshot else {
            return;
        };
        if let Err(e) = write_atomic(path, &text) {
            tracing::warn!(path = %path.display(), error = %e, "月度用量落盘失败（本次计费仅存在于内存）");
        }
    }
}

/// `agent.root` 下的记账文件路径。
pub fn spend_path(root: &Path) -> PathBuf {
    root.join(SPEND_FILE)
}

fn read_ledger(path: &Path) -> Result<Ledger, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Ledger::default()),
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("不是合法 JSON：{e}")),
        // 文件不存在是常态（首次启用），不算错误。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => Err(format!("读取失败：{e}")),
    }
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // rename 在跨卷/被占用时会失败，退回直接覆盖写。
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(path, text).map_err(|_| rename_err)
        }
    }
}

/// 按 `YYYY-MM` 只保留最近 `KEEP_MONTHS` 个月。
fn prune(months: &mut BTreeMap<String, f64>) {
    while months.len() > KEEP_MONTHS {
        let Some(oldest) = months.keys().next().cloned() else {
            break;
        };
        months.remove(&oldest);
    }
}

/// 当前月份键（`YYYY-MM`），跨月自动换新键 → 用量自然按月归零。
fn current_month() -> String {
    month_of(SystemTime::now())
}

fn month_of(when: SystemTime) -> String {
    let secs = when
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, _day) = civil_from_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}")
}

/// 儒略日数 → 公历 (年, 月, 日)（Howard Hinnant 的 civil_from_days 算法）。
///
/// 用它而不是再加一个时间依赖：我们只需要「今天是哪个月」。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_key_follows_the_calendar() {
        let at = |secs: u64| month_of(UNIX_EPOCH + std::time::Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01");
        // 2026-10-24 00:00:00 UTC / 2026-11-01 00:00:00 UTC（跨月必须换键）
        assert_eq!(at(1_792_800_000), "2026-10");
        assert_eq!(at(1_793_491_200), "2026-11");
        // 月末 23:59:59 仍属当月
        assert_eq!(at(1_793_491_200 - 1), "2026-10");
    }

    #[test]
    fn accumulates_and_reloads_from_disk() {
        let dir = std::env::temp_dir().join(format!("agent-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = spend_path(&dir);
        assert!(path.ends_with("budget.json"));

        let store = SpendStore::load(path.clone());
        assert_eq!(store.month_total(), 0.0, "首次启用应从 0 开始");
        store.add(0.5);
        store.add(0.25);
        assert_eq!(store.month_total(), 0.75);

        // 关键回归：换一个实例重新加载，累计值必须还在（跨重启）。
        let reloaded = SpendStore::load(path);
        assert_eq!(reloaded.month_total(), 0.75, "重启后用量被清零 = 熔断失灵");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_and_weird_values_are_ignored() {
        let store = SpendStore::memory_only();
        store.add(0.0);
        store.add(-5.0);
        store.add(f64::NAN);
        store.add(f64::INFINITY);
        assert_eq!(
            store.month_total(),
            0.0,
            "usage 缺失时成本为 0，不该把账记坏"
        );
    }

    #[test]
    fn corrupt_file_does_not_break_startup() {
        let dir = std::env::temp_dir().join(format!("agent-budget-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("应能建目录");
        let path = dir.join(SPEND_FILE);
        std::fs::write(&path, "{ this is not json").expect("应能写脏文件");

        let store = SpendStore::load(path.clone());
        assert_eq!(store.month_total(), 0.0, "损坏时应退回空账本而不是 panic");
        store.add(1.0);
        assert_eq!(store.month_total(), 1.0, "损坏恢复后仍能正常记账");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_only_never_touches_disk() {
        let store = SpendStore::memory_only();
        store.add(3.0);
        assert_eq!(store.month_total(), 3.0);
    }
}
