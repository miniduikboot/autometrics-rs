use linkme::distributed_slice;

// This "distributed slice" is used to collect all the alerts defined when a
// call to the `autometrics` macro has the `alerts` argument.
// The alert definitions are collected into this slice at compile time.
// See https://github.com/dtolnay/linkme for how this works.
#[doc(hidden)]
#[distributed_slice]
pub static METRICS: [Alert] = [..];

#[doc(hidden)]
pub struct Alert {
    pub function: &'static str,
    pub module: &'static str,
    pub success_rate: Option<&'static str>,
    pub latency: Option<(&'static str, &'static str)>,
}

impl Alert {
    fn to_objectives(&self) -> impl Iterator<Item = Box<dyn Objective>> {
        let mut objectives: Vec<Box<dyn Objective>> = vec![];
        if let Some(success_rate) = self.success_rate {
            objectives.push(Box::new(SuccessRateObjective {
                function: self.function,
                module: self.module,
                success_rate,
            }));
        }
        if let Some((latency_threshold, latency_objective)) = self.latency {
            objectives.push(Box::new(LatencyObjective {
                function: self.function,
                module: self.module,
                latency_threshold,
                latency_objective,
            }));
        }
        objectives.into_iter()
    }
}

/// Returns the Prometheus recording and alerting rules as a YAML string.
///
/// To generate alerts, add the `alerts` parameter to the `autometrics` macro
/// for at least one function.
///
/// Then, call this function to generate the Prometheus rules. You will need
/// to output the rules to a file and
/// [load them into Prometheus](https://prometheus.io/docs/prometheus/latest/configuration/recording_rules/).
pub fn generate_alerts() -> String {
    let groups = METRICS
        .iter()
        .flat_map(|alert| {
            alert.to_objectives().flat_map(|objective| {
                [
                    objective.error_ratio_recording_rules(),
                    objective.meta_recording_rules(),
                    objective.alert_rules(),
                ]
                .into_iter()
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "---
# Prometheus recording and alerting rules generated by autometrics-rs

groups:
{groups}"
    )
}

trait Objective {
    fn slo_type(&self) -> &'static str;
    fn function(&self) -> &'static str;
    fn module(&self) -> &'static str;
    fn success_rate(&self) -> &'static str;
    fn error_query(&self, window: &str) -> String;
    fn total_query(&self, window: &str) -> String;

    fn id(&self) -> String {
        format!("{}-{}", self.module(), self.function())
    }

    fn filter_labels(&self) -> String {
        let function = self.function();
        let module = self.module();
        let slo_type = self.slo_type();
        format!("{{function=\"{function}\",module=\"{module}\",objective=\"{slo_type}\"}}")
    }

    /// When we create a new recording rule, attach these labels to it
    fn recording_labels(&self) -> String {
        let slo_type = self.slo_type();
        let function = self.function();
        let module = self.module();
        format!(
            "labels:
      objective: {slo_type}
      function: {function}
      module: {module}",
        )
    }

    /// Create recording rules for the error rate at each window
    fn error_ratio_recording_rules(&self) -> String {
        let id = self.id();
        let slo_type = self.slo_type();
        let recording_labels = self.recording_labels();
        let filter_labels = self.filter_labels();
        let mut rules = format!(
            "- name: autometrics-slo-sli-recordings-{id}-{slo_type}
  rules:\n"
        );

        for window in ["5m", "30m", "1h", "2h", "6h", "1d", "3d"] {
            let errors = self.error_query(window);
            let total = self.total_query(window);
            rules.push_str(&format!(
                "  - record: slo:sli_error:ratio_rate{window}
    expr: {errors} / {total}
    {recording_labels}
      window: {window}\n"
            ));
        }

        // 30d query is a bit different
        rules.push_str(&format!(
            "  - record: slo:sli_error:ratio_rate30d
    expr: |
      sum_over_time(slo:sli_error:ratio_rate5m{filter_labels}[30d])
      / ignoring(window)
      count_over_time(slo:sli_error:ratio_rate5m{filter_labels}[30d])
    {recording_labels}
      window: 30d\n"
        ));

        rules
    }

    /// Create the recording rules for the burn rate and error budget
    fn meta_recording_rules(&self) -> String {
        let recording_labels = self.recording_labels();
        let filter_labels = self.filter_labels();
        let id = self.id();
        let slo_type = self.slo_type();
        let success_rate = self.success_rate();
        format!(
            "- name: autometrics-slo-meta-recordings-{id}-{slo_type}
  rules:
  - record: slo:objective:ratio
    expr: vector({success_rate})
    {recording_labels}
  - record: slo:error_budget:ratio
    expr: vector(1 - {success_rate})
    {recording_labels}
  - record: slo:time_period:days
    expr: vector(30)
    {recording_labels}
  - record: slo:current_burn_rate:ratio
    expr: slo:sli_error:ratio_rate5m{filter_labels} / on(function, module, objective) group_left slo:error_budget:ratio{filter_labels}
    {recording_labels}
  - record: slo:period_burn_rate:ratio
    expr: slo:sli_error:ratio_rate30d{filter_labels} / on(function, module, objective) group_left slo:error_budget:ratio{filter_labels}
    {recording_labels}
  - record: slo:period_error_budget_remaining:ratio
    expr: 1 - slo:period_burn_rate:ratio{filter_labels}
    {recording_labels}\n")
    }

    /// Create the alert definitions for the SLO
    fn alert_rules(&self) -> String {
        let error_rate = format!("(1 - {})", self.success_rate());
        let labels = self.filter_labels();
        let id = self.id();
        let function = self.function();
        let module = self.module();
        let slo_type = self.slo_type();
        format!(
            "- name: autometrics-slo-alerts-{id}-{slo_type}
  rules:
  - alert: HighErrorRate-{id}-{slo_type}
    expr: |
      (
        max(slo:sli_error:ratio_rate5m{labels} > (14.4 * {error_rate}))
        and
        max(slo:sli_error:ratio_rate1h{labels} > (14.4 * {error_rate}))
      )
      or
      (
        max(slo:sli_error:ratio_rate30m{labels} > (6 * {error_rate}))
        and
        max(slo:sli_error:ratio_rate6h{labels} > (6 * {error_rate}))
      )
    labels:
      severity: page
    annotations:
      summary: High error rate for function '{function}' in module '{module}'
      title: (page) '{function}' in module '{module}' SLO error budget burn rate is too fast.
  - alert: HighErrorRate-{id}-{slo_type}
    expr: |
      (
        max(slo:sli_error:ratio_rate2h{labels} > (3 * {error_rate}))
        and
        max(slo:sli_error:ratio_rate1d{labels} > (3 * {error_rate}))
      )
      or
      (
        max(slo:sli_error:ratio_rate6h{labels} > (1 * {error_rate}))
        and
        max(slo:sli_error:ratio_rate3d{labels} > (1 * {error_rate}))
      )
    labels:
      severity: ticket
    annotations:
      summary: High error rate for function '{function}' in module '{module}'
      title: (ticket) '{function}' in module '{module}' SLO error budget burn rate is too fast.\n"
        )
    }
}

struct SuccessRateObjective {
    function: &'static str,
    module: &'static str,
    success_rate: &'static str,
}

impl Objective for SuccessRateObjective {
    fn slo_type(&self) -> &'static str {
        "success-rate"
    }

    fn function(&self) -> &'static str {
        self.function
    }

    fn module(&self) -> &'static str {
        self.module
    }

    fn success_rate(&self) -> &'static str {
        self.success_rate
    }

    fn error_query(&self, window: &str) -> String {
        let function = self.function();
        let module = self.module();
        format!("sum(rate(function_calls_count{{function=\"{function}\",module=\"{module}\",result=\"error\"}}[{window}]))")
    }

    fn total_query(&self, window: &str) -> String {
        let function = self.function();
        let module = self.module();
        format!("sum(rate(function_calls_count{{function=\"{function}\",module=\"{module}\"}}[{window}]))")
    }
}

struct LatencyObjective {
    function: &'static str,
    module: &'static str,
    latency_objective: &'static str,
    latency_threshold: &'static str,
}

impl Objective for LatencyObjective {
    fn slo_type(&self) -> &'static str {
        "latency"
    }

    fn function(&self) -> &'static str {
        self.function
    }

    fn module(&self) -> &'static str {
        self.module
    }

    fn success_rate(&self) -> &'static str {
        self.latency_objective
    }

    fn error_query(&self, window: &str) -> String {
        let function = self.function();
        let module = self.module();
        let latency_threshold = self.latency_threshold;
        format!("(sum(rate(function_calls_duration_bucket{{function=\"{function}\",module=\"{module}\"}}[{window}])) \
                - sum(rate(function_calls_duration_bucket{{le=\"{latency_threshold}\",function=\"{function}\",module=\"{module}\"}}[{window}])))")
    }

    fn total_query(&self, window: &str) -> String {
        let function = self.function();
        let module = self.module();
        format!("sum(rate(function_calls_duration_bucket{{function=\"{function}\",module=\"{module}\"}}[{window}]))")
    }
}