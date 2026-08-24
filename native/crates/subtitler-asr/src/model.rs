use serde::{Deserialize, Serialize};

const MIB_PER_GIB: u64 = 1_024;
const TINY_MIN_MEMORY_MB: u64 = 2 * MIB_PER_GIB;
const BASE_MIN_MEMORY_MB: u64 = 4 * MIB_PER_GIB;
const SMALL_MIN_MEMORY_MB: u64 = 8 * MIB_PER_GIB;
const MEDIUM_MIN_MEMORY_MB: u64 = 16 * MIB_PER_GIB;
const LARGE_V3_TURBO_MIN_MEMORY_MB: u64 = 24 * MIB_PER_GIB;
const Q8_MEDIUM_MIN_MEMORY_MB: u64 = 32 * MIB_PER_GIB;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputeBackend {
    #[default]
    Cpu,
    Cuda,
    Metal,
    Vulkan,
}

impl ComputeBackend {
    /// Whether this backend is an accelerator rather than the portable CPU
    /// implementation. This says nothing about available accelerator memory;
    /// the current profile intentionally does not guess that value.
    pub const fn is_accelerated(self) -> bool {
        !matches!(self, Self::Cpu)
    }

    const fn display_name(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Cuda => "CUDA",
            Self::Metal => "Metal",
            Self::Vulkan => "Vulkan",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModel {
    Tiny,
    Base,
    Small,
    Medium,
    LargeV3Turbo,
}

impl LocalModel {
    /// A conservative lower bound for currently available system memory. The
    /// thresholds deliberately leave room for the OS, browser, media decoder,
    /// and ASR working buffers; they are not model-file sizes.
    pub const fn minimum_available_memory_mb(self) -> u64 {
        match self {
            Self::Tiny => TINY_MIN_MEMORY_MB,
            Self::Base => BASE_MIN_MEMORY_MB,
            Self::Small => SMALL_MIN_MEMORY_MB,
            Self::Medium => MEDIUM_MIN_MEMORY_MB,
            Self::LargeV3Turbo => LARGE_V3_TURBO_MIN_MEMORY_MB,
        }
    }

    /// The minimum logical CPU count for a practical local recommendation.
    /// Accelerators reduce CPU-side orchestration work, but never relax memory
    /// requirements because the profile does not yet report dedicated VRAM.
    pub const fn minimum_logical_cpu_count(self, backend: ComputeBackend) -> u16 {
        match self {
            Self::Tiny => 1,
            Self::Base => 2,
            Self::Small if backend.is_accelerated() => 3,
            Self::Small => 4,
            Self::Medium if backend.is_accelerated() => 4,
            Self::Medium => 8,
            Self::LargeV3Turbo => 8,
        }
    }

    /// The quality-oriented large-v3-turbo recommendation needs a supported
    /// accelerator. It is intentionally not selected for CPU-only profiles,
    /// where a medium model is the more reliable local plan.
    pub const fn requires_accelerated_backend(self) -> bool {
        matches!(self, Self::LargeV3Turbo)
    }

    /// Returns whether this model is a conservative fit for the supplied
    /// profile and backend. A zero CPU or memory reading means the detector did
    /// not produce a usable value, so no model is claimed to be feasible.
    pub fn is_feasible_on(self, profile: &HardwareProfile, backend: ComputeBackend) -> bool {
        profile.supports_backend(backend)
            && (!self.requires_accelerated_backend() || backend.is_accelerated())
            && profile.available_memory_mb >= self.minimum_available_memory_mb()
            && profile.logical_cpu_count >= self.minimum_logical_cpu_count(backend)
    }

    const fn display_name(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Base => "base",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::LargeV3Turbo => "large-v3-turbo",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantization {
    Q5_0,
    #[serde(rename = "q5_k_m")]
    Q5Km,
    Q8_0,
    F16,
}

impl Quantization {
    const fn display_name(self) -> &'static str {
        match self {
            Self::Q5_0 => "q5_0",
            Self::Q5Km => "q5_k_m",
            Self::Q8_0 => "q8_0",
            Self::F16 => "f16",
        }
    }
}

/// A plain-language estimate for the normal extension UI. This is a planning
/// signal, not a throughput guarantee: the actual media codec, language, and
/// competing system load still affect processing speed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalPerformance {
    Excellent,
    Good,
    #[default]
    MayBeSlow,
    CloudHelpful,
}

impl LocalPerformance {
    /// Cloud processing is *only* an optional, explicit user choice. This
    /// flag exists so callers can surface that choice without treating this
    /// recommendation as an upload request.
    pub const fn cloud_may_be_helpful(self) -> bool {
        matches!(self, Self::CloudHelpful)
    }

    const fn user_facing_label(self) -> &'static str {
        match self {
            Self::Excellent => "Excellent local performance",
            Self::Good => "Good local performance",
            Self::MayBeSlow => "Local processing may be slow",
            Self::CloudHelpful => "Cloud processing may be helpful",
        }
    }
}

/// This profile is populated by a platform-specific detector in the native
/// host. The ASR crate only makes a deterministic recommendation from it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub logical_cpu_count: u16,
    pub available_memory_mb: u64,
    #[serde(default)]
    pub supported_backends: Vec<ComputeBackend>,
}

impl HardwareProfile {
    /// CPU is always available as the portable fallback. When several
    /// accelerators are supported, choose a stable priority independent of the
    /// detector's enumeration order: CUDA, Metal, Vulkan, then CPU.
    pub fn preferred_backend(&self) -> ComputeBackend {
        [
            ComputeBackend::Cuda,
            ComputeBackend::Metal,
            ComputeBackend::Vulkan,
            ComputeBackend::Cpu,
        ]
        .into_iter()
        .find(|backend| self.supports_backend(*backend))
        .unwrap_or(ComputeBackend::Cpu)
    }

    /// A profile need not list `cpu`: it is the mandatory fallback. An
    /// accelerator must be explicitly reported by the platform detector.
    pub fn supports_backend(&self, backend: ComputeBackend) -> bool {
        backend == ComputeBackend::Cpu || self.supported_backends.contains(&backend)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRecommendation {
    pub model: LocalModel,
    pub quantization: Quantization,
    pub backend: ComputeBackend,
    /// Plain-language state for normal UI. `cloud_helpful` still describes a
    /// local recommendation; it never initiates cloud processing.
    #[serde(default)]
    pub local_performance: LocalPerformance,
    pub reason: String,
    /// Backward-compatible convenience flag derived solely from
    /// `local_performance`. It is advisory and has no upload side effect.
    pub cloud_may_be_helpful: bool,
}

/// Choose the highest-quality local model that meets conservative CPU and
/// available-memory limits. The function does not inspect providers, network
/// settings, recordings, URLs, credentials, or cloud configuration. It can
/// therefore never start an upload or silently switch the job away from local
/// processing.
pub fn recommend_local_model(profile: &HardwareProfile) -> ModelRecommendation {
    let preferred_backend = profile.preferred_backend();
    let selected = highest_feasible_model(profile, preferred_backend)
        .map(|model| (model, preferred_backend, true))
        // Keep a defensive CPU path in case a future accelerator's model
        // feasibility rules become more restrictive than CPU's rules.
        .or_else(|| {
            (preferred_backend != ComputeBackend::Cpu)
                .then(|| highest_feasible_model(profile, ComputeBackend::Cpu))
                .flatten()
                .map(|model| (model, ComputeBackend::Cpu, true))
        })
        // Incomplete or severely constrained hardware readings still receive
        // an explicit smallest local plan. The caller can present the advisory
        // cloud option, but must obtain consent before using any provider.
        .unwrap_or((LocalModel::Tiny, ComputeBackend::Cpu, false));

    let (model, backend, is_feasible) = selected;
    let local_performance = classify_local_performance(profile, model, backend, is_feasible);
    let quantization = select_quantization(profile, model, backend);

    ModelRecommendation {
        model,
        quantization,
        backend,
        local_performance,
        reason: recommendation_reason(
            profile,
            model,
            quantization,
            backend,
            local_performance,
            is_feasible,
        ),
        cloud_may_be_helpful: local_performance.cloud_may_be_helpful(),
    }
}

fn highest_feasible_model(
    profile: &HardwareProfile,
    backend: ComputeBackend,
) -> Option<LocalModel> {
    [
        LocalModel::LargeV3Turbo,
        LocalModel::Medium,
        LocalModel::Small,
        LocalModel::Base,
        LocalModel::Tiny,
    ]
    .into_iter()
    .find(|model| model.is_feasible_on(profile, backend))
}

fn select_quantization(
    profile: &HardwareProfile,
    model: LocalModel,
    backend: ComputeBackend,
) -> Quantization {
    match model {
        LocalModel::Tiny => Quantization::Q5_0,
        // F16 is deliberately never auto-selected: `HardwareProfile` does not
        // include dedicated accelerator memory, so claiming an F16 fit would
        // be speculative. A high-memory, many-core CPU can safely prefer q8
        // for a medium model without making that assumption.
        LocalModel::Medium
            if backend == ComputeBackend::Cpu
                && profile.available_memory_mb >= Q8_MEDIUM_MIN_MEMORY_MB
                && profile.logical_cpu_count >= 12 =>
        {
            Quantization::Q8_0
        }
        _ => Quantization::Q5Km,
    }
}

fn classify_local_performance(
    profile: &HardwareProfile,
    model: LocalModel,
    backend: ComputeBackend,
    is_feasible: bool,
) -> LocalPerformance {
    // A base model is the minimum reasonably useful local quality tier. If it
    // cannot be recommended, retain the tiny local fallback but make an
    // explicitly consented cloud option visible to the user.
    if !is_feasible || !LocalModel::Base.is_feasible_on(profile, backend) {
        return LocalPerformance::CloudHelpful;
    }

    match (model, backend) {
        (LocalModel::LargeV3Turbo, accelerated) if accelerated.is_accelerated() => {
            LocalPerformance::Excellent
        }
        (LocalModel::Medium, accelerated)
            if accelerated.is_accelerated() && profile.logical_cpu_count >= 4 =>
        {
            LocalPerformance::Excellent
        }
        (LocalModel::Medium, ComputeBackend::Cpu)
            if profile.logical_cpu_count >= 12
                && profile.available_memory_mb >= LARGE_V3_TURBO_MIN_MEMORY_MB =>
        {
            LocalPerformance::Excellent
        }
        (LocalModel::Small | LocalModel::Medium | LocalModel::LargeV3Turbo, _) => {
            LocalPerformance::Good
        }
        (LocalModel::Tiny | LocalModel::Base, _) => LocalPerformance::MayBeSlow,
    }
}

fn recommendation_reason(
    profile: &HardwareProfile,
    model: LocalModel,
    quantization: Quantization,
    backend: ComputeBackend,
    local_performance: LocalPerformance,
    is_feasible: bool,
) -> String {
    let privacy_note =
        "Local processing stays on this device; cloud processing is not selected or initiated and remains an optional, explicit choice.";
    if !is_feasible {
        return format!(
            "Hardware information is incomplete or below the conservative local threshold; selecting {model} with {quantization} on {backend} as the safest local starting point. {privacy_note}",
            model = model.display_name(),
            quantization = quantization.display_name(),
            backend = backend.display_name(),
        );
    }

    format!(
        "Selected {model} with {quantization} on {backend}: the highest-quality local configuration that fits the detected {memory_mb} MB available memory and {cpu_count} logical CPU cores. {performance}. {privacy_note}",
        model = model.display_name(),
        quantization = quantization.display_name(),
        backend = backend.display_name(),
        memory_mb = profile.available_memory_mb,
        cpu_count = profile.logical_cpu_count,
        performance = local_performance.user_facing_label(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        logical_cpu_count: u16,
        available_memory_mb: u64,
        supported_backends: Vec<ComputeBackend>,
    ) -> HardwareProfile {
        HardwareProfile {
            logical_cpu_count,
            available_memory_mb,
            supported_backends,
        }
    }

    #[test]
    fn backend_priority_is_stable_and_ignores_detector_order() {
        let profile = profile(
            8,
            LARGE_V3_TURBO_MIN_MEMORY_MB,
            vec![
                ComputeBackend::Vulkan,
                ComputeBackend::Cpu,
                ComputeBackend::Metal,
                ComputeBackend::Cuda,
            ],
        );

        assert_eq!(profile.preferred_backend(), ComputeBackend::Cuda);
        assert_eq!(
            recommend_local_model(&profile).backend,
            ComputeBackend::Cuda
        );
    }

    #[test]
    fn backend_priority_uses_metal_then_vulkan_and_always_falls_back_to_cpu() {
        assert_eq!(
            profile(
                8,
                MEDIUM_MIN_MEMORY_MB,
                vec![ComputeBackend::Vulkan, ComputeBackend::Metal]
            )
            .preferred_backend(),
            ComputeBackend::Metal
        );
        assert_eq!(
            profile(8, MEDIUM_MIN_MEMORY_MB, vec![ComputeBackend::Vulkan]).preferred_backend(),
            ComputeBackend::Vulkan
        );
        assert_eq!(
            profile(8, MEDIUM_MIN_MEMORY_MB, Vec::new()).preferred_backend(),
            ComputeBackend::Cpu
        );
    }

    #[test]
    fn memory_boundaries_select_the_largest_safe_cpu_model() {
        let cases = [
            (BASE_MIN_MEMORY_MB - 1, LocalModel::Tiny),
            (BASE_MIN_MEMORY_MB, LocalModel::Base),
            (SMALL_MIN_MEMORY_MB - 1, LocalModel::Base),
            (SMALL_MIN_MEMORY_MB, LocalModel::Small),
            (MEDIUM_MIN_MEMORY_MB - 1, LocalModel::Small),
            (MEDIUM_MIN_MEMORY_MB, LocalModel::Medium),
        ];

        for (available_memory_mb, expected_model) in cases {
            let recommendation = recommend_local_model(&profile(8, available_memory_mb, vec![]));
            assert_eq!(
                recommendation.model, expected_model,
                "memory boundary {available_memory_mb} MB"
            );
            assert_eq!(recommendation.backend, ComputeBackend::Cpu);
        }
    }

    #[test]
    fn large_v3_turbo_requires_both_acceleration_and_its_exact_boundaries() {
        let at_boundary = recommend_local_model(&profile(
            8,
            LARGE_V3_TURBO_MIN_MEMORY_MB,
            vec![ComputeBackend::Cuda],
        ));
        assert_eq!(at_boundary.model, LocalModel::LargeV3Turbo);
        assert_eq!(at_boundary.backend, ComputeBackend::Cuda);
        assert_eq!(at_boundary.local_performance, LocalPerformance::Excellent);

        assert_eq!(
            recommend_local_model(&profile(
                8,
                LARGE_V3_TURBO_MIN_MEMORY_MB - 1,
                vec![ComputeBackend::Cuda],
            ))
            .model,
            LocalModel::Medium
        );
        assert_eq!(
            recommend_local_model(&profile(
                7,
                LARGE_V3_TURBO_MIN_MEMORY_MB,
                vec![ComputeBackend::Cuda]
            ))
            .model,
            LocalModel::Medium
        );
        assert_eq!(
            recommend_local_model(&profile(12, LARGE_V3_TURBO_MIN_MEMORY_MB, vec![])).model,
            LocalModel::Medium
        );
    }

    #[test]
    fn cpu_boundaries_cap_models_even_when_memory_is_abundant() {
        let cases = [
            (1, LocalModel::Tiny),
            (2, LocalModel::Base),
            (3, LocalModel::Base),
            (4, LocalModel::Small),
            (7, LocalModel::Small),
            (8, LocalModel::Medium),
        ];

        for (logical_cpu_count, expected_model) in cases {
            assert_eq!(
                recommend_local_model(&profile(logical_cpu_count, Q8_MEDIUM_MIN_MEMORY_MB, vec![]))
                    .model,
                expected_model,
                "CPU boundary {logical_cpu_count} logical cores"
            );
        }
    }

    #[test]
    fn acceleration_relaxes_cpu_orchestration_not_memory_requirements() {
        assert_eq!(
            recommend_local_model(&profile(3, SMALL_MIN_MEMORY_MB, vec![ComputeBackend::Cuda]))
                .model,
            LocalModel::Small
        );
        assert_eq!(
            recommend_local_model(&profile(
                4,
                MEDIUM_MIN_MEMORY_MB,
                vec![ComputeBackend::Cuda]
            ))
            .model,
            LocalModel::Medium
        );
        assert_eq!(
            recommend_local_model(&profile(
                4,
                MEDIUM_MIN_MEMORY_MB - 1,
                vec![ComputeBackend::Cuda]
            ))
            .model,
            LocalModel::Small
        );
    }

    #[test]
    fn model_feasibility_rejects_unreported_backends_and_incomplete_hardware() {
        let complete_cpu = profile(8, MEDIUM_MIN_MEMORY_MB, vec![]);
        assert!(LocalModel::Medium.is_feasible_on(&complete_cpu, ComputeBackend::Cpu));
        assert!(!LocalModel::Medium.is_feasible_on(&complete_cpu, ComputeBackend::Cuda));
        assert!(!LocalModel::LargeV3Turbo.is_feasible_on(&complete_cpu, ComputeBackend::Cpu));

        assert!(!LocalModel::Tiny.is_feasible_on(
            &profile(0, TINY_MIN_MEMORY_MB, vec![ComputeBackend::Cuda]),
            ComputeBackend::Cuda,
        ));
        assert!(!LocalModel::Tiny.is_feasible_on(
            &profile(1, 0, vec![ComputeBackend::Cuda]),
            ComputeBackend::Cuda,
        ));
    }

    #[test]
    fn quality_first_plan_uses_q8_only_for_a_high_memory_many_core_cpu_medium() {
        let high_end_cpu = recommend_local_model(&profile(12, Q8_MEDIUM_MIN_MEMORY_MB, vec![]));
        assert_eq!(high_end_cpu.model, LocalModel::Medium);
        assert_eq!(high_end_cpu.quantization, Quantization::Q8_0);
        assert_eq!(high_end_cpu.local_performance, LocalPerformance::Excellent);

        let accelerated = recommend_local_model(&profile(
            12,
            Q8_MEDIUM_MIN_MEMORY_MB,
            vec![ComputeBackend::Cuda],
        ));
        assert_eq!(accelerated.model, LocalModel::LargeV3Turbo);
        assert_eq!(accelerated.quantization, Quantization::Q5Km);

        for recommendation in [high_end_cpu, accelerated] {
            assert_ne!(recommendation.quantization, Quantization::F16);
        }
    }

    #[test]
    fn performance_states_cover_excellent_good_may_be_slow_and_cloud_helpful() {
        assert_eq!(
            recommend_local_model(&profile(
                4,
                MEDIUM_MIN_MEMORY_MB,
                vec![ComputeBackend::Metal]
            ))
            .local_performance,
            LocalPerformance::Excellent
        );
        assert_eq!(
            recommend_local_model(&profile(4, SMALL_MIN_MEMORY_MB, vec![])).local_performance,
            LocalPerformance::Good
        );
        assert_eq!(
            recommend_local_model(&profile(2, BASE_MIN_MEMORY_MB, vec![])).local_performance,
            LocalPerformance::MayBeSlow
        );
        assert_eq!(
            recommend_local_model(&profile(1, Q8_MEDIUM_MIN_MEMORY_MB, vec![])).local_performance,
            LocalPerformance::CloudHelpful
        );
    }

    #[test]
    fn cloud_helpful_is_advisory_and_never_replaces_the_local_plan() {
        let recommendation = recommend_local_model(&profile(1, TINY_MIN_MEMORY_MB, vec![]));

        assert_eq!(recommendation.model, LocalModel::Tiny);
        assert_eq!(recommendation.quantization, Quantization::Q5_0);
        assert_eq!(recommendation.backend, ComputeBackend::Cpu);
        assert_eq!(
            recommendation.local_performance,
            LocalPerformance::CloudHelpful
        );
        assert!(recommendation.cloud_may_be_helpful);
        assert!(recommendation.reason.contains("not selected or initiated"));
        assert!(recommendation.reason.contains("explicit choice"));
    }

    #[test]
    fn cloud_flag_is_derived_only_from_the_user_facing_performance_state() {
        for recommendation in [
            recommend_local_model(&profile(
                4,
                MEDIUM_MIN_MEMORY_MB,
                vec![ComputeBackend::Cuda],
            )),
            recommend_local_model(&profile(4, SMALL_MIN_MEMORY_MB, vec![])),
            recommend_local_model(&profile(2, BASE_MIN_MEMORY_MB, vec![])),
            recommend_local_model(&profile(1, TINY_MIN_MEMORY_MB, vec![])),
        ] {
            assert_eq!(
                recommendation.cloud_may_be_helpful,
                recommendation.local_performance.cloud_may_be_helpful()
            );
        }
    }
}
