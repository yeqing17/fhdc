//! 设备注册表:唯一有权分配连接代数的组件。
//!
//! `hdc list targets` 只输出连接键,没有 transport id,因此代数在
//! 键消失/重现或状态变化时递增;仅元数据变化(理论上只有状态)不影响
//! 已有在线键的代数。UI 捕获选中时刻的 `DeviceTarget`,后端在每个
//! 目标绑定命令前校验代数,陈旧目标直接失败。

use std::collections::HashMap;

use fhdc_domain::{
    BridgeError, ConnectKey, DeviceDescriptor, DeviceRecord, DeviceSnapshot, DeviceTarget,
    ErrorCode,
};

#[derive(Debug, Default)]
pub struct DeviceRegistry {
    records: HashMap<ConnectKey, DeviceRecord>,
    generations: HashMap<ConnectKey, u64>,
    selected: Option<ConnectKey>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用最新发现的设备列表对账,返回快照。选中设备消失时清除选择。
    pub fn reconcile(&mut self, descriptors: Vec<DeviceDescriptor>) -> DeviceSnapshot {
        let mut seen = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            let key = descriptor.key.clone();
            match self.records.get_mut(&key) {
                Some(record) => {
                    if record.descriptor.state != descriptor.state {
                        let generation = self
                            .generations
                            .get_mut(&key)
                            .expect("record without generation");
                        *generation += 1;
                        record.generation = *generation;
                    }
                    record.descriptor = descriptor;
                }
                None => {
                    let entry = self.generations.entry(key.clone()).or_insert(1);
                    *entry += 1;
                    let record = DeviceRecord {
                        descriptor,
                        generation: *entry,
                    };
                    self.records.insert(key.clone(), record);
                }
            }
            let record = self.records.get(&key).expect("just inserted");
            seen.push(DeviceRecord {
                descriptor: record.descriptor.clone(),
                generation: record.generation,
            });
        }
        self.records
            .retain(|key, _| seen.iter().any(|record| &record.descriptor.key == key));
        if let Some(selected) = self.selected.clone()
            && !seen.iter().any(|record| record.descriptor.key == selected)
        {
            self.selected = None;
        }
        self.snapshot()
    }

    /// 显式选择设备;拒绝未知键,从不自动选择。
    pub fn select(&mut self, key: Option<ConnectKey>) -> Result<DeviceSnapshot, BridgeError> {
        if let Some(key) = &key
            && !self.records.contains_key(key)
        {
            return Err(BridgeError::new(
                ErrorCode::DeviceNotFound,
                "未发现该设备,可能已断开",
            ));
        }
        self.selected = key;
        Ok(self.snapshot())
    }

    pub fn snapshot(&self) -> DeviceSnapshot {
        let mut devices: Vec<DeviceRecord> = self.records.values().cloned().collect();
        // HashMap 迭代顺序由随机种子决定;发现轮询每 2 秒重建一次快照,
        // 不排序的话设备下拉框顺序会持续抖动。在线设备置顶,其余按键序。
        devices.sort_by(|a, b| {
            b.descriptor
                .state
                .is_online()
                .cmp(&a.descriptor.state.is_online())
                .then_with(|| a.descriptor.key.cmp(&b.descriptor.key))
        });
        DeviceSnapshot {
            devices,
            selected: self.selected.clone(),
        }
    }

    /// 当前选中且在线设备的目标;设备离线时返回 None。
    pub fn current_online_target(&self) -> Option<DeviceTarget> {
        let key = self.selected.as_ref()?;
        let record = self.records.get(key)?;
        if !record.descriptor.state.is_online() {
            return None;
        }
        Some(DeviceTarget {
            key: key.clone(),
            generation: record.generation,
        })
    }

    /// 校验目标仍是同一条连接且在线;否则 None(调用方转成 DeviceUnavailable)。
    pub fn current_online(&self, target: &DeviceTarget) -> Option<&DeviceRecord> {
        let record = self.records.get(&target.key)?;
        (record.generation == target.generation && record.descriptor.state.is_online())
            .then_some(record)
    }

    pub fn generation(&self, key: &ConnectKey) -> Option<u64> {
        self.records.get(key).map(|record| record.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fhdc_domain::DeviceState;

    fn descriptor(key: &str, state: DeviceState) -> DeviceDescriptor {
        DeviceDescriptor {
            key: ConnectKey::new(key).unwrap(),
            state,
        }
    }

    #[test]
    fn generation_bumps_on_reappear() {
        let mut registry = DeviceRegistry::new();
        let snap = registry.reconcile(vec![descriptor("KEY_A", DeviceState::Connected)]);
        let first = snap.devices[0].generation;
        let snap = registry.reconcile(vec![]);
        assert!(snap.devices.is_empty());
        let snap = registry.reconcile(vec![descriptor("KEY_A", DeviceState::Connected)]);
        assert_eq!(snap.devices[0].generation, first + 1);
    }

    #[test]
    fn generation_bumps_on_state_change() {
        let mut registry = DeviceRegistry::new();
        let snap = registry.reconcile(vec![descriptor("KEY_A", DeviceState::Connected)]);
        let first = snap.devices[0].generation;
        let snap = registry.reconcile(vec![descriptor("KEY_A", DeviceState::Offline)]);
        assert_eq!(snap.devices[0].generation, first + 1);
    }

    #[test]
    fn selection_clears_when_device_detaches() {
        let mut registry = DeviceRegistry::new();
        registry.reconcile(vec![descriptor("KEY_A", DeviceState::Connected)]);
        let key = ConnectKey::new("KEY_A").unwrap();
        registry.select(Some(key.clone())).unwrap();
        let snap = registry.reconcile(vec![]);
        assert_eq!(snap.selected, None);
        assert!(registry.select(Some(key)).is_err());
    }

    #[test]
    fn stale_target_rejected() {
        let mut registry = DeviceRegistry::new();
        let snap = registry.reconcile(vec![descriptor("KEY_A", DeviceState::Connected)]);
        let stale = snap.devices[0].target();
        registry.reconcile(vec![]);
        registry.reconcile(vec![descriptor("KEY_A", DeviceState::Connected)]);
        assert!(registry.current_online(&stale).is_none());
        let key = ConnectKey::new("KEY_A").unwrap();
        registry.select(Some(key)).unwrap();
        let fresh = registry.snapshot().devices[0].target();
        assert!(registry.current_online(&fresh).is_some());
        assert_eq!(registry.current_online_target(), Some(fresh));
    }

    #[test]
    fn snapshot_order_is_stable_and_online_first() {
        // 同一组设备反复生成快照,顺序必须一致:在线置顶,其余按连接键。
        let mut registry = DeviceRegistry::new();
        registry.reconcile(vec![
            descriptor("KEY_C", DeviceState::Connected),
            descriptor("KEY_A", DeviceState::Offline),
            descriptor("KEY_B", DeviceState::Connected),
        ]);
        let first: Vec<String> = registry
            .snapshot()
            .devices
            .iter()
            .map(|record| record.descriptor.key.as_str().to_owned())
            .collect();
        for _ in 0..16 {
            let again: Vec<String> = registry
                .snapshot()
                .devices
                .iter()
                .map(|record| record.descriptor.key.as_str().to_owned())
                .collect();
            assert_eq!(again, first);
        }
        assert_eq!(first, vec!["KEY_B", "KEY_C", "KEY_A"]);
    }
}
