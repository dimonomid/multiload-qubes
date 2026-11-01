//! Chart names, saved keys and default order. Rendering uses ChartKind too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChartKind {
    Cpu,
    QubeCpu,
    Ram,
    QubeRam,
    Network,
    Swap,
    Disk,
}

pub(crate) struct ChartMetadata {
    pub kind: ChartKind,
    pub name: &'static str,
    pub key: &'static str,
    pub default_enabled: bool,
    pub theme_group: &'static str,
    // Older settings numbered palette groups in renderer order.
    pub palette_id: usize,
}

use ChartKind::*;
// This is also the default display order. Keep it in enum order for array indexing.
pub(crate) const CHARTS: [ChartMetadata; 7] = [
    ChartMetadata {
        kind: Cpu,
        name: "CPU",
        key: "cpu",
        default_enabled: true,
        theme_group: "CPU",
        palette_id: 0,
    },
    ChartMetadata {
        kind: QubeCpu,
        name: "Per-qube CPU",
        key: "qube_cpu",
        default_enabled: false,
        theme_group: "PerQubeCPU",
        palette_id: 6,
    },
    ChartMetadata {
        kind: Ram,
        name: "RAM",
        key: "ram",
        default_enabled: true,
        theme_group: "RAM",
        palette_id: 1,
    },
    ChartMetadata {
        kind: QubeRam,
        name: "Per-qube RAM",
        key: "qube_ram",
        default_enabled: false,
        theme_group: "PerQubeRAM",
        palette_id: 5,
    },
    ChartMetadata {
        kind: Network,
        name: "Network",
        key: "network",
        default_enabled: true,
        theme_group: "Network",
        palette_id: 2,
    },
    ChartMetadata {
        kind: Swap,
        name: "Per-qube swap",
        key: "swap",
        default_enabled: true,
        theme_group: "Swap",
        palette_id: 3,
    },
    ChartMetadata {
        kind: Disk,
        name: "Disk I/O",
        key: "disk",
        default_enabled: true,
        theme_group: "Disk",
        palette_id: 4,
    },
];
impl ChartKind {
    pub const COUNT: usize = CHARTS.len();
    pub const ALL: [Self; Self::COUNT] = {
        let mut kinds = [Self::Cpu; Self::COUNT];
        let mut i = 0;
        while i < Self::COUNT {
            assert!(CHARTS[i].kind as usize == i);
            kinds[i] = CHARTS[i].kind;
            i += 1;
        }
        kinds
    };
    pub const fn index(self) -> usize {
        self as usize
    }
    pub fn metadata(self) -> &'static ChartMetadata {
        &CHARTS[self.index()]
    }
    pub fn from_key(key: &str) -> Option<Self> {
        CHARTS
            .iter()
            .find(|chart| chart.key == key)
            .map(|chart| chart.kind)
    }
    pub fn is_per_qube(self) -> bool {
        matches!(self, Self::QubeCpu | Self::QubeRam | Self::Swap)
    }
    pub fn is_io(self) -> bool {
        matches!(self, Self::Network | Self::Disk)
    }
}
