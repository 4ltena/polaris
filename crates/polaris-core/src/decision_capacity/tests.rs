//! Synthetic capacity edges and domain separation; no model or GPU is started.
use super::*;

fn fixture() -> (
    ProfileMemory,
    CapacityObservation,
    RunFootprint,
    CapacityPolicy,
) {
    let target = MemoryTarget {
        device_id: "gpu-1".into(),
        memory: MemoryKind::DedicatedVram,
        compute: ComputeKind::Gpu,
    };
    (
        ProfileMemory {
            accepted: true,
            target: target.clone(),
            max_context_tokens: 1024,
            max_parallel: 1,
            peak: Some(MemoryPeak {
                bytes: 5 * GIB,
                evidence: PeakEvidence::Measured,
            }),
        },
        CapacityObservation {
            target,
            total_bytes: Some(8 * GIB),
            available_bytes: Some(8 * GIB),
            working_set_limit_bytes: None,
            captured_at: Instant::now(),
        },
        RunFootprint {
            context_tokens: 1024,
            parallel: 1,
        },
        CapacityPolicy {
            reserve_bytes: 2 * GIB,
            colocated_bytes: GIB,
            budget_limit_bytes: None,
            max_age: Duration::from_secs(15),
        },
    )
}

fn unknown(reason: UnverifiedCapacity) -> CapacityAssessment {
    CapacityAssessment::Unverified { reason }
}

#[test]
fn recommendations_cover_intermediate_sizes_without_hiding_other_families() {
    assert_eq!(CapacityTier::from_bytes(None), None);
    assert_eq!(CapacityTier::from_bytes(Some(0)), None);
    for (bytes, tier) in [
        (4 * GIB, CapacityTier::Small),
        (8 * GIB, CapacityTier::Small),
        (16 * GIB - 1, CapacityTier::Small),
        (16 * GIB, CapacityTier::Medium),
        (24 * GIB - 1, CapacityTier::Medium),
        (24 * GIB, CapacityTier::Large),
        (80 * GIB, CapacityTier::Large),
    ] {
        let found = CapacityTier::from_bytes(Some(bytes)).unwrap();
        assert_eq!(found, tier);
        assert_eq!(found.local_candidates().len(), 4);
        for name in ["Clef", "Clef-flash", "SemIf", "System One compatible"] {
            assert!(found.local_candidates().contains(&name));
        }
    }
    assert_eq!(CapacityTier::Small.local_candidates()[0], "SemIf");
    assert_eq!(CapacityTier::Medium.local_candidates()[0], "Clef-flash");
    assert_eq!(CapacityTier::Large.local_candidates()[0], "Clef");
}

#[test]
fn peak_reserve_and_colocation_fit_at_the_exact_byte_edge() {
    let (profile, mut observed, request, policy) = fixture();
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        CapacityAssessment::Fits {
            budget_bytes: 8 * GIB,
            required_bytes: 8 * GIB,
            peak_evidence: PeakEvidence::Measured
        }
    );
    observed.available_bytes = Some(8 * GIB - 1);
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        CapacityAssessment::Insufficient {
            budget_bytes: 8 * GIB - 1,
            required_bytes: 8 * GIB
        }
    );
    observed.available_bytes = Some(0);
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        CapacityAssessment::Insufficient {
            budget_bytes: 0,
            required_bytes: 8 * GIB
        }
    );
}

#[test]
fn an_unknown_reading_is_not_zero_or_a_promise_that_the_model_fits() {
    let (mut profile, mut observed, request, policy) = fixture();
    observed.available_bytes = None;
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        unknown(UnverifiedCapacity::AvailableMemory)
    );
    observed.available_bytes = Some(9 * GIB);
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        unknown(UnverifiedCapacity::Observation)
    );
    observed.available_bytes = Some(8 * GIB);
    observed.total_bytes = None;
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        unknown(UnverifiedCapacity::Observation)
    );
    observed.total_bytes = Some(8 * GIB);
    profile.peak = None;
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        unknown(UnverifiedCapacity::Peak)
    );
}

#[test]
fn cpu_rss_and_other_devices_never_admit_a_gpu_profile() {
    let (profile, observed, request, policy) = fixture();
    for target in [
        MemoryTarget {
            compute: ComputeKind::Cpu,
            ..observed.target.clone()
        },
        MemoryTarget {
            memory: MemoryKind::UnifiedMemory,
            ..observed.target.clone()
        },
        MemoryTarget {
            device_id: "gpu-2".into(),
            ..observed.target.clone()
        },
        MemoryTarget {
            device_id: String::new(),
            ..observed.target.clone()
        },
    ] {
        let changed = CapacityObservation {
            target,
            ..observed.clone()
        };
        assert_eq!(
            assess(&profile, &changed, request, policy, changed.captured_at),
            unknown(UnverifiedCapacity::Target)
        );
    }
    for (memory, compute) in [
        (MemoryKind::DedicatedVram, ComputeKind::Cpu),
        (MemoryKind::SystemRam, ComputeKind::Gpu),
    ] {
        let mut changed = observed.clone();
        changed.target.memory = memory;
        changed.target.compute = compute;
        let mut impossible = profile.clone();
        impossible.target = changed.target.clone();
        assert_eq!(
            assess(&impossible, &changed, request, policy, changed.captured_at),
            unknown(UnverifiedCapacity::Target)
        );
    }
}

#[test]
fn unified_gpu_needs_a_separate_working_set_and_honors_the_smallest_budget() {
    let (mut profile, mut observed, request, mut policy) = fixture();
    profile.target.memory = MemoryKind::UnifiedMemory;
    observed.target = profile.target.clone();
    observed.total_bytes = Some(24 * GIB);
    observed.available_bytes = Some(16 * GIB);
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        unknown(UnverifiedCapacity::WorkingSet)
    );
    observed.working_set_limit_bytes = Some(8 * GIB);
    assert!(
        matches!(assess(&profile, &observed, request, policy, observed.captured_at), CapacityAssessment::Fits { budget_bytes, .. } if budget_bytes == 8*GIB)
    );
    policy.budget_limit_bytes = Some(7 * GIB);
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        CapacityAssessment::Insufficient {
            budget_bytes: 7 * GIB,
            required_bytes: 8 * GIB
        }
    );
    policy.budget_limit_bytes = None;
    observed.available_bytes = Some(6 * GIB);
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        CapacityAssessment::Insufficient {
            budget_bytes: 6 * GIB,
            required_bytes: 8 * GIB
        }
    );
}

#[test]
fn acceptance_shape_and_freshness_are_required_even_with_unlimited_memory() {
    let (mut profile, observed, request, policy) = fixture();
    profile.accepted = false;
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        unknown(UnverifiedCapacity::Profile)
    );
    profile.accepted = true;
    for oversized in [
        RunFootprint {
            context_tokens: 1025,
            ..request
        },
        RunFootprint {
            parallel: 2,
            ..request
        },
        RunFootprint {
            context_tokens: 0,
            ..request
        },
    ] {
        assert_eq!(
            assess(&profile, &observed, oversized, policy, observed.captured_at),
            unknown(UnverifiedCapacity::RequestShape)
        );
    }
    assert!(matches!(
        assess(
            &profile,
            &observed,
            request,
            policy,
            observed.captured_at + policy.max_age
        ),
        CapacityAssessment::Fits { .. }
    ));
    assert_eq!(
        assess(
            &profile,
            &observed,
            request,
            policy,
            observed.captured_at + policy.max_age + Duration::from_nanos(1)
        ),
        unknown(UnverifiedCapacity::StaleObservation)
    );
    assert_eq!(
        assess(
            &profile,
            &observed,
            request,
            policy,
            observed.captured_at - Duration::from_secs(1)
        ),
        unknown(UnverifiedCapacity::Observation)
    );
}

#[test]
fn an_estimate_stays_labelled_and_arithmetic_never_wraps_to_a_fit() {
    let (mut profile, observed, request, mut policy) = fixture();
    profile.peak.as_mut().unwrap().evidence = PeakEvidence::ValidatedUpperBound;
    assert!(matches!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        CapacityAssessment::Fits {
            peak_evidence: PeakEvidence::ValidatedUpperBound,
            ..
        }
    ));
    policy.colocated_bytes = u64::MAX;
    assert_eq!(
        assess(&profile, &observed, request, policy, observed.captured_at),
        unknown(UnverifiedCapacity::Arithmetic)
    );
}

#[test]
fn diagnostics_do_not_call_ram_vram_or_claim_model_acceptance() {
    let (_, mut observed, _, _) = fixture();
    observed.target.memory = MemoryKind::UnifiedMemory;
    observed.total_bytes = Some(24 * GIB);
    let text = observation::diagnostic_lines(&observed).join("\n");
    assert!(text.contains("unified memory (not dedicated VRAM)"));
    assert!(text.contains("24 GiB or more"));
    assert!(text.contains("Clef > Clef-flash > SemIf > System One compatible"));
    assert!(text.contains("Hosted Jev/remote System One"));
    assert!(text.contains("not verified here"));
    observed.target.memory = MemoryKind::SystemRam;
    let text = observation::diagnostic_lines(&observed).join("\n");
    assert!(text.contains("system RAM (not GPU memory)"));
    assert!(!text.contains("Recommendation hint:"));
}

#[test]
fn the_native_hint_never_fills_in_an_unobserved_available_budget() {
    let observed = observation::host_memory();
    assert_eq!(observed.available_bytes, None);
    assert_eq!(observed.working_set_limit_bytes, None);
    assert_eq!(observed.target.compute, ComputeKind::Cpu);
    assert!(observed.total_bytes.is_none_or(|bytes| bytes > 0));
}

#[test]
fn a_cpu_budget_is_not_a_gpu_measurement_or_an_unbounded_ram_claim() {
    let observed = observation::cpu_budget();
    assert_eq!(observed.target.compute, ComputeKind::Cpu);
    assert_eq!(observed.working_set_limit_bytes, None);
    if let Some(free) = observed.available_bytes {
        assert!(observed.total_bytes.is_some_and(|total| free <= total));
    }
}
