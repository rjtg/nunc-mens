//! Timing for the standalone scale benchmark. The default build elides every span.

#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Repository,
    SearchSetup,
    ModelLoad,
    EmbedQuery,
    IndexRead,
    SourceFilter,
    VisibilityFilter,
    VectorScan,
    KeywordScan,
    Fusion,
    Context,
    Freshness,
    Format,
}

#[cfg(feature = "search-profile")]
mod enabled {
    use super::Stage;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::io::{self, Write};
    use std::time::Instant;

    impl Stage {
        const ALL: [Stage; 13] = [
            Self::Repository,
            Self::SearchSetup,
            Self::ModelLoad,
            Self::EmbedQuery,
            Self::IndexRead,
            Self::SourceFilter,
            Self::VisibilityFilter,
            Self::VectorScan,
            Self::KeywordScan,
            Self::Fusion,
            Self::Context,
            Self::Freshness,
            Self::Format,
        ];

        fn name(self) -> &'static str {
            match self {
                Self::Repository => "repository",
                Self::SearchSetup => "search_setup",
                Self::ModelLoad => "model_load",
                Self::EmbedQuery => "embed_query",
                Self::IndexRead => "index_read",
                Self::SourceFilter => "source_filter",
                Self::VisibilityFilter => "visibility_filter",
                Self::VectorScan => "vector_scan",
                Self::KeywordScan => "keyword_scan",
                Self::Fusion => "fusion",
                Self::Context => "context",
                Self::Freshness => "freshness",
                Self::Format => "format",
            }
        }
    }

    #[derive(Clone, Copy)]
    struct Sample {
        nanoseconds: u128,
        calls: usize,
    }

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static SAMPLES: RefCell<[Sample; Stage::ALL.len()]> = const {
            RefCell::new([Sample { nanoseconds: 0, calls: 0 }; Stage::ALL.len()])
        };
    }

    pub(crate) struct Session(Instant);

    pub(crate) fn begin() -> Option<Session> {
        if std::env::var_os("NUNC_SEARCH_PROFILE").as_deref() != Some("1".as_ref()) {
            return None;
        }
        ACTIVE.set(true);
        Some(Session(Instant::now()))
    }

    impl Drop for Session {
        fn drop(&mut self) {
            ACTIVE.set(false);
            let total = self.0.elapsed().as_nanos();
            let report = SAMPLES.with_borrow_mut(|samples| {
                let mut report = BTreeMap::new();
                for stage in Stage::ALL {
                    let sample = &mut samples[stage as usize];
                    if sample.calls != 0 {
                        report.insert(stage.name(), sample.nanoseconds);
                        sample.calls = 0;
                        sample.nanoseconds = 0;
                    }
                }
                report.insert("cli", total);
                report
            });
            if let Ok(json) = serde_json::to_string(&report) {
                let _ = writeln!(io::stderr().lock(), "nunc-profile {json}");
            }
        }
    }

    pub(crate) struct Span(Stage, Instant);

    pub(crate) fn span(stage: Stage) -> Option<Span> {
        ACTIVE.get().then(|| Span(stage, Instant::now()))
    }

    impl Drop for Span {
        fn drop(&mut self) {
            let elapsed = self.1.elapsed().as_nanos();
            SAMPLES.with_borrow_mut(|samples| {
                let sample = &mut samples[self.0 as usize];
                sample.nanoseconds += elapsed;
                sample.calls += 1;
            });
        }
    }
}

#[cfg(feature = "search-profile")]
pub(crate) use enabled::{begin, span};

#[cfg(not(feature = "search-profile"))]
pub(crate) struct Disabled;
#[cfg(not(feature = "search-profile"))]
impl Drop for Disabled {
    fn drop(&mut self) {}
}
#[cfg(not(feature = "search-profile"))]
pub(crate) fn begin() -> Disabled {
    Disabled
}
#[cfg(not(feature = "search-profile"))]
pub(crate) fn span(_: Stage) -> Disabled {
    Disabled
}

pub(crate) fn measure<T>(stage: Stage, f: impl FnOnce() -> T) -> T {
    let _span = span(stage);
    f()
}
