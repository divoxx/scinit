use super::process_harness::{ProcessTestHarness, TestProcess};
use anyhow::{Context, Result};
use nix::sys::signal::Signal;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, debug, warn};

/// Framework for comprehensive performance testing and benchmarking
pub struct PerformanceTestFramework {
    harness: ProcessTestHarness,
    performance_baselines: HashMap<String, PerformanceBaseline>,
    regression_thresholds: HashMap<String, f64>,
}

impl PerformanceTestFramework {
    /// Create a new performance testing framework
    pub fn new(harness: ProcessTestHarness) -> Self {
        let mut performance_baselines = HashMap::new();
        let mut regression_thresholds = HashMap::new();
        
        // Set performance baselines (these would typically be measured from known good versions)
        performance_baselines.insert("signal_response".to_string(), PerformanceBaseline {
            mean_duration: Duration::from_millis(50),
            p95_duration: Duration::from_millis(100),
            p99_duration: Duration::from_millis(150),
        });
        
        performance_baselines.insert("process_spawn".to_string(), PerformanceBaseline {
            mean_duration: Duration::from_millis(75),
            p95_duration: Duration::from_millis(150),
            p99_duration: Duration::from_millis(200),
        });
        
        performance_baselines.insert("graceful_shutdown".to_string(), PerformanceBaseline {
            mean_duration: Duration::from_millis(200),
            p95_duration: Duration::from_millis(500),
            p99_duration: Duration::from_millis(1000),
        });
        
        // Set regression thresholds (% degradation that triggers a failure)
        regression_thresholds.insert("signal_response".to_string(), 0.5); // 50% degradation
        regression_thresholds.insert("process_spawn".to_string(), 0.3);   // 30% degradation
        regression_thresholds.insert("graceful_shutdown".to_string(), 0.4); // 40% degradation
        
        Self {
            harness,
            performance_baselines,
            regression_thresholds,
        }
    }

    /// Run comprehensive performance benchmark suite
    pub async fn run_performance_benchmark(&mut self) -> Result<PerformanceBenchmarkResult> {
        
        let benchmark_start = Instant::now();
        
        // Benchmark 1: Signal Response Performance
        let signal_benchmark = self.benchmark_signal_response().await?;
        
        // Benchmark 2: Process Spawning Performance
        let spawn_benchmark = self.benchmark_process_spawn().await?;
        
        // Benchmark 3: Graceful Shutdown Performance
        let shutdown_benchmark = self.benchmark_graceful_shutdown().await?;
        
        // Benchmark 4: Memory Usage Performance
        let memory_benchmark = self.benchmark_memory_usage().await?;
        
        // Benchmark 5: CPU Usage Performance
        let cpu_benchmark = self.benchmark_cpu_usage().await?;
        
        let total_benchmark_duration = benchmark_start.elapsed();
        
        // Analyze for regressions
        let regression_analysis = self.analyze_regressions(&[
            ("signal_response", &signal_benchmark.statistics),
            ("process_spawn", &spawn_benchmark.statistics),
            ("graceful_shutdown", &shutdown_benchmark.statistics),
        ]);
        
        Ok(PerformanceBenchmarkResult {
            signal_response: signal_benchmark,
            process_spawn: spawn_benchmark,
            graceful_shutdown: shutdown_benchmark,
            memory_usage: memory_benchmark,
            cpu_usage: cpu_benchmark,
            regression_analysis,
            total_benchmark_duration,
            benchmark_passed: regression_analysis.regressions_detected.is_empty(),
        })
    }

    /// Benchmark signal response performance
    async fn benchmark_signal_response(&mut self) -> Result<SignalResponseBenchmark> {
        
        let iterations = 50;
        let mut measurements = Vec::new();
        
        for i in 0..iterations {
            debug!("Signal response benchmark iteration {}/{}", i + 1, iterations);
            
            let mut process = self.harness.spawn_scinit(&["sleep", "10"]).await?;
            tokio::time::sleep(Duration::from_millis(100)).await; // Let process start
            
            let signal_time = Instant::now();
            nix::sys::signal::kill(process.pid, Signal::SIGTERM)?;
            
            let exit_status = process.wait_for_exit_timeout(Duration::from_secs(2)).await?;
            let response_time = signal_time.elapsed();
            
            measurements.push(SignalResponseMeasurement {
                response_time,
                successful: exit_status.is_some(),
                iteration: i + 1,
            });
            
            // Small delay between iterations
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        
        let statistics = self.calculate_performance_statistics(&measurements.iter()
            .map(|m| m.response_time)
            .collect::<Vec<_>>());
        
        Ok(SignalResponseBenchmark {
            measurements,
            statistics,
            iterations: iterations as u32,
        })
    }

    /// Benchmark process spawning performance
    async fn benchmark_process_spawn(&mut self) -> Result<ProcessSpawnBenchmark> {
        
        let iterations = 30;
        let mut measurements = Vec::new();
        
        for i in 0..iterations {
            debug!("Process spawn benchmark iteration {}/{}", i + 1, iterations);
            
            let spawn_start = Instant::now();
            let mut process = self.harness.spawn_scinit(&["sleep", "0.5"]).await?;
            let spawn_duration = spawn_start.elapsed();
            
            tokio::time::sleep(Duration::from_millis(100)).await; // Let process start
            let process_running = process.is_running();
            
            measurements.push(ProcessSpawnMeasurement {
                spawn_duration,
                successful: process_running,
                iteration: i + 1,
            });
            
            // Clean up process
            let _ = nix::sys::signal::kill(process.pid, Signal::SIGTERM);
            let _ = process.wait_for_exit_timeout(Duration::from_secs(1)).await;
            
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        
        let statistics = self.calculate_performance_statistics(&measurements.iter()
            .map(|m| m.spawn_duration)
            .collect::<Vec<_>>());
        
        Ok(ProcessSpawnBenchmark {
            measurements,
            statistics,
            iterations: iterations as u32,
        })
    }

    /// Benchmark graceful shutdown performance
    async fn benchmark_graceful_shutdown(&mut self) -> Result<GracefulShutdownBenchmark> {
        
        let iterations = 30;
        let mut measurements = Vec::new();
        
        for i in 0..iterations {
            debug!("Graceful shutdown benchmark iteration {}/{}", i + 1, iterations);
            
            let mut process = self.harness.spawn_scinit(&["sleep", "10"]).await?;
            tokio::time::sleep(Duration::from_millis(200)).await; // Let process start
            
            let shutdown_start = Instant::now();
            nix::sys::signal::kill(process.pid, Signal::SIGTERM)?;
            
            let exit_status = process.wait_for_exit_timeout(Duration::from_secs(3)).await?;
            let shutdown_duration = shutdown_start.elapsed();
            
            measurements.push(GracefulShutdownMeasurement {
                shutdown_duration,
                successful: exit_status.is_some(),
                iteration: i + 1,
            });
            
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        
        let statistics = self.calculate_performance_statistics(&measurements.iter()
            .map(|m| m.shutdown_duration)
            .collect::<Vec<_>>());
        
        Ok(GracefulShutdownBenchmark {
            measurements,
            statistics,
            iterations: iterations as u32,
        })
    }

    /// Benchmark memory usage
    async fn benchmark_memory_usage(&mut self) -> Result<MemoryUsageBenchmark> {
        
        let mut process = self.harness.spawn_scinit(&["sleep", "5"]).await?;
        tokio::time::sleep(Duration::from_millis(500)).await; // Let process stabilize
        
        let mut memory_samples = Vec::new();
        let sample_duration = Duration::from_secs(3);
        let sample_interval = Duration::from_millis(100);
        
        let sampling_start = Instant::now();
        while sampling_start.elapsed() < sample_duration {
            let memory_usage = self.measure_memory_usage(process.pid).await?;
            memory_samples.push(MemoryUsageSample {
                timestamp: sampling_start.elapsed(),
                rss_kb: memory_usage.rss_kb,
                vss_kb: memory_usage.vss_kb,
            });
            
            tokio::time::sleep(sample_interval).await;
        }
        
        // Clean up
        let _ = nix::sys::signal::kill(process.pid, Signal::SIGTERM);
        let _ = process.wait_for_exit_timeout(Duration::from_secs(1)).await;
        
        let peak_rss = memory_samples.iter().map(|s| s.rss_kb).max().unwrap_or(0);
        let average_rss = memory_samples.iter().map(|s| s.rss_kb).sum::<u64>() / memory_samples.len() as u64;
        
        Ok(MemoryUsageBenchmark {
            samples: memory_samples,
            peak_rss_kb: peak_rss,
            average_rss_kb: average_rss,
            sample_count: memory_samples.len(),
        })
    }

    /// Benchmark CPU usage
    async fn benchmark_cpu_usage(&mut self) -> Result<CpuUsageBenchmark> {
        info!("Benchmarking CPU usage");
        
        let mut process = self.harness.spawn_scinit(&["sleep", "3"]).await?;
        tokio::time::sleep(Duration::from_millis(500)).await; // Let process stabilize
        
        let mut cpu_samples = Vec::new();
        let sample_duration = Duration::from_secs(2);
        let sample_interval = Duration::from_millis(100);
        
        let sampling_start = Instant::now();
        while sampling_start.elapsed() < sample_duration {
            let cpu_usage = self.measure_cpu_usage(process.pid).await?;
            cpu_samples.push(CpuUsageSample {
                timestamp: sampling_start.elapsed(),
                cpu_percent: cpu_usage,
            });
            
            tokio::time::sleep(sample_interval).await;
        }
        
        // Clean up
        let _ = nix::sys::signal::kill(process.pid, Signal::SIGTERM);
        let _ = process.wait_for_exit_timeout(Duration::from_secs(1)).await;
        
        let peak_cpu = cpu_samples.iter().map(|s| s.cpu_percent).fold(0.0_f64, f64::max);
        let average_cpu = cpu_samples.iter().map(|s| s.cpu_percent).sum::<f64>() / cpu_samples.len() as f64;
        
        Ok(CpuUsageBenchmark {
            samples: cpu_samples,
            peak_cpu_percent: peak_cpu,
            average_cpu_percent: average_cpu,
            sample_count: cpu_samples.len(),
        })
    }

    /// Calculate performance statistics from a set of duration measurements
    fn calculate_performance_statistics(&self, measurements: &[Duration]) -> PerformanceStatistics {
        if measurements.is_empty() {
            return PerformanceStatistics {
                min: Duration::ZERO,
                max: Duration::ZERO,
                mean: Duration::ZERO,
                p50: Duration::ZERO,
                p95: Duration::ZERO,
                p99: Duration::ZERO,
                sample_count: 0,
            };
        }
        
        let mut sorted_measurements = measurements.to_vec();
        sorted_measurements.sort();
        
        let min = sorted_measurements[0];
        let max = sorted_measurements[sorted_measurements.len() - 1];
        let mean = Duration::from_nanos(
            measurements.iter()
                .map(|d| d.as_nanos())
                .sum::<u128>() / measurements.len() as u128
        );
        
        let p50_idx = (sorted_measurements.len() as f64 * 0.50) as usize;
        let p95_idx = (sorted_measurements.len() as f64 * 0.95) as usize;
        let p99_idx = (sorted_measurements.len() as f64 * 0.99) as usize;
        
        PerformanceStatistics {
            min,
            max,
            mean,
            p50: sorted_measurements[p50_idx.min(sorted_measurements.len() - 1)],
            p95: sorted_measurements[p95_idx.min(sorted_measurements.len() - 1)],
            p99: sorted_measurements[p99_idx.min(sorted_measurements.len() - 1)],
            sample_count: measurements.len(),
        }
    }

    /// Analyze performance for regressions
    fn analyze_regressions(&self, benchmarks: &[(&str, &PerformanceStatistics)]) -> RegressionAnalysis {
        let mut regressions_detected = Vec::new();
        
        for (benchmark_name, statistics) in benchmarks {
            if let Some(baseline) = self.performance_baselines.get(*benchmark_name) {
                let threshold = self.regression_thresholds.get(*benchmark_name).copied().unwrap_or(0.5);
                
                // Check for regression in mean performance
                let mean_regression = (statistics.mean.as_nanos() as f64 - baseline.mean_duration.as_nanos() as f64) 
                    / baseline.mean_duration.as_nanos() as f64;
                
                if mean_regression > threshold {
                    regressions_detected.push(PerformanceRegression {
                        benchmark_name: benchmark_name.to_string(),
                        metric: "mean".to_string(),
                        baseline_value: baseline.mean_duration,
                        measured_value: statistics.mean,
                        regression_percentage: mean_regression * 100.0,
                        threshold_percentage: threshold * 100.0,
                    });
                }
                
                // Check for regression in P95 performance
                let p95_regression = (statistics.p95.as_nanos() as f64 - baseline.p95_duration.as_nanos() as f64) 
                    / baseline.p95_duration.as_nanos() as f64;
                
                if p95_regression > threshold {
                    regressions_detected.push(PerformanceRegression {
                        benchmark_name: benchmark_name.to_string(),
                        metric: "p95".to_string(),
                        baseline_value: baseline.p95_duration,
                        measured_value: statistics.p95,
                        regression_percentage: p95_regression * 100.0,
                        threshold_percentage: threshold * 100.0,
                    });
                }
            }
        }
        
        RegressionAnalysis {
            regressions_detected,
            total_benchmarks_analyzed: benchmarks.len(),
        }
    }

    /// Measure memory usage for a process
    async fn measure_memory_usage(&self, pid: nix::unistd::Pid) -> Result<MemoryUsage> {
        let stat_path = format!("/proc/{}/status", pid);
        let status_content = tokio::fs::read_to_string(&stat_path).await
            .context("Failed to read process status")?;
        
        let mut rss_kb = 0;
        let mut vss_kb = 0;
        
        for line in status_content.lines() {
            if line.starts_with("VmRSS:") {
                if let Some(value_str) = line.split_whitespace().nth(1) {
                    rss_kb = value_str.parse().unwrap_or(0);
                }
            } else if line.starts_with("VmSize:") {
                if let Some(value_str) = line.split_whitespace().nth(1) {
                    vss_kb = value_str.parse().unwrap_or(0);
                }
            }
        }
        
        Ok(MemoryUsage { rss_kb, vss_kb })
    }

    /// Measure CPU usage for a process (simplified implementation)
    async fn measure_cpu_usage(&self, pid: nix::unistd::Pid) -> Result<f64> {
        let stat_path = format!("/proc/{}/stat", pid);
        let stat_content = tokio::fs::read_to_string(&stat_path).await
            .context("Failed to read process stat")?;
        
        // This is a simplified CPU measurement - in practice you'd need to
        // measure over time intervals and calculate percentage based on system ticks
        let fields: Vec<&str> = stat_content.split_whitespace().collect();
        
        if fields.len() > 15 {
            // Fields 13 and 14 are utime and stime (user and system CPU time)
            let utime: u64 = fields[13].parse().unwrap_or(0);
            let stime: u64 = fields[14].parse().unwrap_or(0);
            
            // This is a placeholder calculation - real CPU % would require time-based sampling
            let total_time = (utime + stime) as f64;
            Ok(total_time / 10000.0) // Simplified percentage
        } else {
            Ok(0.0)
        }
    }
}

// Performance data structures

/// Performance baseline for comparison
#[derive(Debug, Clone)]
pub struct PerformanceBaseline {
    pub mean_duration: Duration,
    pub p95_duration: Duration,
    pub p99_duration: Duration,
}

/// Comprehensive performance statistics
#[derive(Debug, Clone)]
pub struct PerformanceStatistics {
    pub min: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub sample_count: usize,
}

/// Complete benchmark result
#[derive(Debug)]
pub struct PerformanceBenchmarkResult {
    pub signal_response: SignalResponseBenchmark,
    pub process_spawn: ProcessSpawnBenchmark,
    pub graceful_shutdown: GracefulShutdownBenchmark,
    pub memory_usage: MemoryUsageBenchmark,
    pub cpu_usage: CpuUsageBenchmark,
    pub regression_analysis: RegressionAnalysis,
    pub total_benchmark_duration: Duration,
    pub benchmark_passed: bool,
}

/// Signal response benchmark results
#[derive(Debug)]
pub struct SignalResponseBenchmark {
    pub measurements: Vec<SignalResponseMeasurement>,
    pub statistics: PerformanceStatistics,
    pub iterations: u32,
}

/// Individual signal response measurement
#[derive(Debug)]
pub struct SignalResponseMeasurement {
    pub response_time: Duration,
    pub successful: bool,
    pub iteration: usize,
}

/// Process spawn benchmark results
#[derive(Debug)]
pub struct ProcessSpawnBenchmark {
    pub measurements: Vec<ProcessSpawnMeasurement>,
    pub statistics: PerformanceStatistics,
    pub iterations: u32,
}

/// Individual process spawn measurement
#[derive(Debug)]
pub struct ProcessSpawnMeasurement {
    pub spawn_duration: Duration,
    pub successful: bool,
    pub iteration: usize,
}

/// Graceful shutdown benchmark results
#[derive(Debug)]
pub struct GracefulShutdownBenchmark {
    pub measurements: Vec<GracefulShutdownMeasurement>,
    pub statistics: PerformanceStatistics,
    pub iterations: u32,
}

/// Individual graceful shutdown measurement
#[derive(Debug)]
pub struct GracefulShutdownMeasurement {
    pub shutdown_duration: Duration,
    pub successful: bool,
    pub iteration: usize,
}

/// Memory usage benchmark results
#[derive(Debug)]
pub struct MemoryUsageBenchmark {
    pub samples: Vec<MemoryUsageSample>,
    pub peak_rss_kb: u64,
    pub average_rss_kb: u64,
    pub sample_count: usize,
}

/// Individual memory usage sample
#[derive(Debug)]
pub struct MemoryUsageSample {
    pub timestamp: Duration,
    pub rss_kb: u64,
    pub vss_kb: u64,
}

/// Memory usage measurement
#[derive(Debug)]
pub struct MemoryUsage {
    pub rss_kb: u64,
    pub vss_kb: u64,
}

/// CPU usage benchmark results
#[derive(Debug)]
pub struct CpuUsageBenchmark {
    pub samples: Vec<CpuUsageSample>,
    pub peak_cpu_percent: f64,
    pub average_cpu_percent: f64,
    pub sample_count: usize,
}

/// Individual CPU usage sample
#[derive(Debug)]
pub struct CpuUsageSample {
    pub timestamp: Duration,
    pub cpu_percent: f64,
}

/// Regression analysis results
#[derive(Debug)]
pub struct RegressionAnalysis {
    pub regressions_detected: Vec<PerformanceRegression>,
    pub total_benchmarks_analyzed: usize,
}

/// Individual performance regression
#[derive(Debug)]
pub struct PerformanceRegression {
    pub benchmark_name: String,
    pub metric: String,
    pub baseline_value: Duration,
    pub measured_value: Duration,
    pub regression_percentage: f64,
    pub threshold_percentage: f64,
}