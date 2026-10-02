//! Request-scoped compile options and the authoritative compile observation
//! (zccache#1550).
//!
//! [`ZccacheService::compile`] and [`ZccacheService::compile_streaming`] keep
//! their signatures and delegate here with [`CompileOptions::default`], which
//! preserves the pre-#1550 environment/default behaviour exactly.

use super::{
    AdmissionDisposition, AdmissionReason, CacheOutcome, CompileChunk, CompileRequest,
    CompileResponse, Result, TestHarnessAdmission, ZccacheService,
};

/// Per-request options for [`ZccacheService::compile_with_options`].
///
/// Construct with [`CompileOptions::default`] and the `with_*` setters; new
/// options are added with defaults that keep existing behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompileOptions {
    /// Admission policy for a rustc `--test` harness, for this request only.
    ///
    /// `None` keeps today's behaviour: `ZCCACHE_CACHE_TEST_BINS` from the
    /// request's forwarded environment, else from the service process. Never
    /// mutates the environment or any service state.
    pub test_harness_admission: Option<TestHarnessAdmission>,
}

impl CompileOptions {
    /// The default options: today's environment/default behaviour.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            test_harness_admission: None,
        }
    }

    /// Select the `--test` harness admission policy for this request.
    #[must_use]
    pub const fn with_test_harness_admission(mut self, admission: TestHarnessAdmission) -> Self {
        self.test_harness_admission = Some(admission);
        self
    }
}

/// What the service decided and measured for one compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompileObservation {
    /// Hit, miss, or compiler error.
    pub cache_outcome: CacheOutcome,
    /// Whether the compile's artifacts were admitted to the cache.
    pub admission: AdmissionDisposition,
    /// Stable categorical reason for [`Self::admission`].
    pub reason: AdmissionReason,
    /// Logical compiler-artifact bytes: the sum of the canonical rustc output
    /// plan's file sizes (the `ArtifactIndex` total) that were stored, served,
    /// or — for a skipped `--test` harness — produced. Excludes
    /// stdout/stderr, index metadata, and archive or filesystem overhead.
    /// `None` when no stage measured them (a failed compile, a non-rustc
    /// bypass).
    pub logical_artifact_bytes: Option<u64>,
}

/// A [`CompileResponse`] together with its [`CompileObservation`].
#[derive(Debug, Clone)]
pub struct ObservedCompileResponse {
    /// The response [`ZccacheService::compile`] would have returned.
    pub response: CompileResponse,
    /// The admission and storage observation for this request.
    pub observation: CompileObservation,
}

impl ZccacheService {
    /// [`Self::compile`] with per-request [`CompileOptions`], returning the
    /// authoritative [`CompileObservation`] beside the response.
    pub async fn compile_with_options(
        &self,
        request: CompileRequest,
        options: CompileOptions,
    ) -> Result<ObservedCompileResponse> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut done = None;
        let observation = self
            .compile_streaming_with_options(request, options, |chunk| match chunk {
                CompileChunk::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                CompileChunk::Stderr(bytes) => stderr.extend_from_slice(&bytes),
                CompileChunk::Done {
                    exit_code,
                    cached,
                    cache_outcome,
                    compile_id,
                    child_memory,
                } => done = Some((exit_code, cached, cache_outcome, compile_id, child_memory)),
            })
            .await?;
        let (exit_code, cached, cache_outcome, compile_id, child_memory) =
            done.ok_or_else(|| {
                super::EmbeddedError::Compile(
                    "streaming compile completed without a Done event".to_string(),
                )
            })?;
        Ok(ObservedCompileResponse {
            response: CompileResponse {
                exit_code,
                stdout,
                stderr,
                cached,
                cache_outcome,
                compile_id,
                child_memory,
            },
            observation,
        })
    }

    /// [`Self::compile_streaming`] with per-request [`CompileOptions`].
    /// Returns the request's [`CompileObservation`] after the terminal
    /// `Done` event has been delivered.
    pub async fn compile_streaming_with_options<F>(
        &self,
        request: CompileRequest,
        options: CompileOptions,
        mut on_chunk: F,
    ) -> Result<CompileObservation>
    where
        F: FnMut(CompileChunk),
    {
        const CHUNK_BYTES: usize = 64 * 1024;
        let (sender, mut receiver) = kernal_api::async_engine::channel(8);
        let context = crate::daemon::compile_output::OutputContext::new(sender);
        let compile = crate::daemon::compile_output::scope(
            context.clone(),
            self.compile_inner(request, options),
        );
        let mut compile = std::pin::pin!(compile);

        let observed = loop {
            let chunk = receiver.recv();
            let mut chunk = std::pin::pin!(chunk);
            match kernal_api::biased_race!((chunk.as_mut()), (compile.as_mut())).await {
                kernal_api::async_engine::BiasedRace2::First(chunk) => {
                    if let Some(chunk) = chunk {
                        emit_output_chunk(&mut on_chunk, chunk);
                    }
                }
                kernal_api::async_engine::BiasedRace2::Second(result) => break result?,
            }
        };
        while let Ok(chunk) = receiver.try_recv() {
            emit_output_chunk(&mut on_chunk, chunk);
        }

        let ObservedCompileResponse {
            response,
            observation,
        } = observed;
        if !context.was_live() {
            for chunk in response.stdout.chunks(CHUNK_BYTES) {
                on_chunk(CompileChunk::Stdout(chunk.to_vec()));
            }
            for chunk in response.stderr.chunks(CHUNK_BYTES) {
                on_chunk(CompileChunk::Stderr(chunk.to_vec()));
            }
        }
        on_chunk(CompileChunk::Done {
            exit_code: response.exit_code,
            cached: response.cached,
            cache_outcome: response.cache_outcome,
            compile_id: response.compile_id,
            child_memory: response.child_memory,
        });
        Ok(observation)
    }
}

fn emit_output_chunk<F>(on_chunk: &mut F, chunk: crate::daemon::compile_output::OutputChunk)
where
    F: FnMut(CompileChunk),
{
    match chunk {
        crate::daemon::compile_output::OutputChunk::Stdout(bytes) => {
            on_chunk(CompileChunk::Stdout(bytes));
        }
        crate::daemon::compile_output::OutputChunk::Stderr(bytes) => {
            on_chunk(CompileChunk::Stderr(bytes));
        }
    }
}

#[cfg(test)]
#[path = "test_harness_admission_tests.rs"]
mod tests;
