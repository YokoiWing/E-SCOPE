use ordered_float::NotNan;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimingSense {
    PositiveUnate,
    NegativeUnate,
    NonUnate,
}

pub const DEFAULT_PRIMARY_INPUT_DRIVER_CELL: &str = "BUFx2_ASAP7_6t_L";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrimaryInputDriverTiming {
    /// Load-dependent driving-cell delay relative to the same cell at zero
    /// output load. This matches Genus `set_driving_cell` Drv Adjust.
    pub arrival_adjust_ps: [f64; 2],
    pub output_slew_ps: [f64; 2],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lut2D {
    pub index_1: Vec<NotNan<f64>>,
    pub index_2: Vec<NotNan<f64>>,
    pub values: Vec<Vec<NotNan<f64>>>,
}

/// One NLDM lookup together with the piecewise-bilinear partial derivatives
/// used by the white-box circuit adjoint.  The value is intentionally the
/// same `NotNan` type returned by the frozen lookup API.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LutLookupWithPartials {
    pub value: NotNan<f64>,
    pub d_slew: f64,
    pub d_load: f64,
}

impl Lut2D {
    fn validate_dimensions(&self) -> Result<(), String> {
        if self.index_1.len() < 2
            || self.index_2.len() < 2
            || self.values.len() != self.index_1.len()
            || self.values.iter().any(|row| row.len() != self.index_2.len())
        {
            return Err("invalid NLDM LUT dimensions".into());
        }
        Ok(())
    }

    fn bracket(axis: &[NotNan<f64>], value: NotNan<f64>) -> usize {
        if value < axis[0] {
            1
        } else if value > axis[axis.len() - 1] {
            axis.len() - 1
        } else {
            axis.iter()
                .position(|entry| *entry >= value)
                .unwrap_or(axis.len() - 1)
                .max(1)
        }
    }

    pub fn lookup(&self, slew: NotNan<f64>, load: NotNan<f64>) -> Result<NotNan<f64>, String> {
        self.validate_dimensions()?;
        Ok(self.lookup_prevalidated(slew, load))
    }

    fn lookup_prevalidated(&self, slew: NotNan<f64>, load: NotNan<f64>) -> NotNan<f64> {
        let xi=Self::bracket(&self.index_1,slew); let yi=Self::bracket(&self.index_2,load);
        let (x0,x1)=(self.index_1[xi-1],self.index_1[xi]); let (y0,y1)=(self.index_2[yi-1],self.index_2[yi]);
        let wx=if x1==x0 {NotNan::new(1.0).unwrap()} else {(x1-slew)/(x1-x0)};
        let wy=if y1==y0 {NotNan::new(1.0).unwrap()} else {(y1-load)/(y1-y0)};
        let one=NotNan::new(1.0).unwrap();
        self.values[xi-1][yi-1]*wx*wy + self.values[xi-1][yi]*wx*(one-wy)
            + self.values[xi][yi-1]*(one-wx)*wy + self.values[xi][yi]*(one-wx)*(one-wy)
    }

    /// Cross-crate convenience wrapper that avoids exposing the workspace's
    /// two historical `ordered-float` versions at a binary boundary.
    pub fn lookup_f64(&self, slew: f64, load: f64) -> Result<f64, String> {
        let slew = NotNan::new(slew).map_err(|_| "NaN NLDM slew")?;
        let load = NotNan::new(load).map_err(|_| "NaN NLDM load")?;
        Ok(self.lookup(slew, load)?.into_inner())
    }

    /// Fast path for tables that were validated once when constructing the
    /// circuit evaluator. It performs the identical interpolation arithmetic
    /// without rescanning immutable dimensions on every lookup.
    pub fn lookup_f64_prevalidated(&self, slew: f64, load: f64) -> Result<f64, String> {
        if slew.is_nan() {
            return Err("NaN NLDM slew".into());
        }
        if load.is_nan() {
            return Err("NaN NLDM load".into());
        }
        let bracket = |axis: &[NotNan<f64>], value: f64| {
            if value < axis[0].into_inner() {
                1
            } else if value > axis[axis.len() - 1].into_inner() {
                axis.len() - 1
            } else {
                axis.iter()
                    .position(|entry| entry.into_inner() >= value)
                    .unwrap_or(axis.len() - 1)
                    .max(1)
            }
        };
        let xi = bracket(&self.index_1, slew);
        let yi = bracket(&self.index_2, load);
        let (x0, x1) = (
            self.index_1[xi - 1].into_inner(),
            self.index_1[xi].into_inner(),
        );
        let (y0, y1) = (
            self.index_2[yi - 1].into_inner(),
            self.index_2[yi].into_inner(),
        );
        let wx = if x1 == x0 {
            1.0
        } else {
            (x1 - slew) / (x1 - x0)
        };
        let wy = if y1 == y0 {
            1.0
        } else {
            (y1 - load) / (y1 - y0)
        };
        let q00 = self.values[xi - 1][yi - 1].into_inner();
        let q01 = self.values[xi - 1][yi].into_inner();
        let q10 = self.values[xi][yi - 1].into_inner();
        let q11 = self.values[xi][yi].into_inner();
        Ok(q00 * wx * wy
            + q01 * wx * (1.0 - wy)
            + q10 * (1.0 - wx) * wy
            + q11 * (1.0 - wx) * (1.0 - wy))
    }

    /// Frozen-value lookup plus analytic partial derivatives of the exact
    /// piecewise-bilinear interpolation/extrapolation used by `lookup`.
    pub fn lookup_with_partials(
        &self,
        slew: NotNan<f64>,
        load: NotNan<f64>,
    ) -> Result<LutLookupWithPartials, String> {
        self.validate_dimensions()?;
        let xi = Self::bracket(&self.index_1, slew);
        let yi = Self::bracket(&self.index_2, load);
        let (x0, x1) = (self.index_1[xi - 1], self.index_1[xi]);
        let (y0, y1) = (self.index_2[yi - 1], self.index_2[yi]);
        let one = NotNan::new(1.0).unwrap();
        let wx = if x1 == x0 { one } else { (x1 - slew) / (x1 - x0) };
        let wy = if y1 == y0 { one } else { (y1 - load) / (y1 - y0) };
        let q00 = self.values[xi - 1][yi - 1];
        let q01 = self.values[xi - 1][yi];
        let q10 = self.values[xi][yi - 1];
        let q11 = self.values[xi][yi];
        let value = q00 * wx * wy
            + q01 * wx * (one - wy)
            + q10 * (one - wx) * wy
            + q11 * (one - wx) * (one - wy);
        let d_slew = if x1 == x0 {
            0.0
        } else {
            (((q10 - q00) * wy + (q11 - q01) * (one - wy)) / (x1 - x0)).into_inner()
        };
        let d_load = if y1 == y0 {
            0.0
        } else {
            (((q01 - q00) * wx + (q11 - q10) * (one - wx)) / (y1 - y0)).into_inner()
        };
        if !d_slew.is_finite() || !d_load.is_finite() {
            return Err("non-finite NLDM LUT partial derivative".into());
        }
        Ok(LutLookupWithPartials { value, d_slew, d_load })
    }

    /// Convert the legacy `[index_1, index_2, value rows...]` representation
    /// used for internal-power tables without changing its stored values.
    pub fn from_indexed_rows(rows: &[Vec<NotNan<f64>>]) -> Result<Self, String> {
        if rows.len() < 4 {
            return Err("invalid indexed NLDM table".into());
        }
        let result = Self {
            index_1: rows[0].clone(),
            index_2: rows[1].clone(),
            values: rows[2..].to_vec(),
        };
        result.validate_dimensions()?;
        Ok(result)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimingArc {
    pub related_pin: String,
    pub timing_sense: TimingSense,
    pub when: Option<String>,
    pub cell_rise: Lut2D,
    pub cell_fall: Lut2D,
    pub rise_transition: Lut2D,
    pub fall_transition: Lut2D,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NLDM {
    pub area: NotNan<f64>,                       // 面积
    pub leakage_power: NotNan<f64>,              // 漏电功率
    pub max_capacitance: NotNan<f64>,            // 最大电容
    // Liberty/NLDM tables are immutable after parsing. Arc keeps the public
    // lookup semantics while making cross-egraph/candidate clones O(1).
    pub pin_order: Arc<Vec<String>>,             // 引脚顺序
    pub pin_info: Arc<HashMap<String, (NotNan<f64>, NotNan<f64>)>>, // 引脚信息 .0负载 .1最大转换时间
    pub delay: Arc<HashMap<String, Vec<Vec<NotNan<f64>>>>>, // 9x7 矩阵，表示延迟
    pub transition: Arc<HashMap<String, Vec<Vec<NotNan<f64>>>>>, // 9x7 矩阵，表示转换时间
    pub internal_power: Arc<HashMap<String, Vec<Vec<NotNan<f64>>>>>, // 9x7 矩阵，表示内部功率
    pub timing_arcs: Arc<Vec<TimingArc>>,
    /// Output Boolean truth table in `pin_order[1..]` bit order.  An empty
    /// table means the Liberty output function was unavailable.
    pub logic_truth_table: Arc<Vec<bool>>,
}

impl NLDM {
    /// Evaluate the mapped primary-input driver in the same form used by
    /// graph timing: rise/fall load-dependent arrival adjustment and output
    /// slew. Delay is referenced to zero load because external STA reports
    /// the driving cell as a `Drv Adjust`, not as a full data-path cell.
    pub fn primary_input_driver_timing(
        &self,
        driver_input_slew_ps: f64,
        output_load_ff: f64,
    ) -> Result<PrimaryInputDriverTiming, String> {
        if !driver_input_slew_ps.is_finite()
            || driver_input_slew_ps < 0.0
            || !output_load_ff.is_finite()
            || output_load_ff < 0.0
        {
            return Err("driver slew/load must be finite and non-negative".into());
        }
        let mut arrival: [Option<f64>; 2] = [None, None];
        let mut transition: [Option<f64>; 2] = [None, None];
        for arc in self.timing_arcs.iter() {
            let mappings: &[(usize, usize)] = match arc.timing_sense {
                TimingSense::PositiveUnate => &[(0, 0), (1, 1)],
                TimingSense::NegativeUnate => &[(1, 0), (0, 1)],
                TimingSense::NonUnate => &[(0, 0), (1, 0), (0, 1), (1, 1)],
            };
            for &(_, output_edge) in mappings {
                let (delay_lut, transition_lut) = if output_edge == 0 {
                    (&arc.cell_rise, &arc.rise_transition)
                } else {
                    (&arc.cell_fall, &arc.fall_transition)
                };
                let loaded_delay =
                    delay_lut.lookup_f64(driver_input_slew_ps, output_load_ff)?;
                let unloaded_delay = delay_lut.lookup_f64(driver_input_slew_ps, 0.0)?;
                let adjustment = loaded_delay - unloaded_delay;
                let slew = transition_lut.lookup_f64(driver_input_slew_ps, output_load_ff)?;
                arrival[output_edge] = Some(
                    arrival[output_edge]
                        .map_or(adjustment, |current| current.max(adjustment)),
                );
                transition[output_edge] = Some(
                    transition[output_edge].map_or(slew, |current| current.max(slew)),
                );
            }
        }
        Ok(PrimaryInputDriverTiming {
            arrival_adjust_ps: [
                arrival[0].ok_or("driver has no rising timing arc")?,
                arrival[1].ok_or("driver has no falling timing arc")?,
            ],
            output_slew_ps: [
                transition[0].ok_or("driver has no rising transition arc")?,
                transition[1].ok_or("driver has no falling transition arc")?,
            ],
        })
    }

    pub fn new(
        area: NotNan<f64>,
        leakage_power: NotNan<f64>,
        max_capacitance: NotNan<f64>,
        pin_order: Vec<String>,
        pin_info: HashMap<String, (NotNan<f64>, NotNan<f64>)>,
        delay: HashMap<String,Vec<Vec<NotNan<f64>>>>,
        transition: HashMap<String,Vec<Vec<NotNan<f64>>>>,
        internal_power: HashMap<String,Vec<Vec<NotNan<f64>>>>,
        timing_arcs: Vec<TimingArc>,
        logic_truth_table: Vec<bool>,
    ) -> Self {
        NLDM {
            area,
            leakage_power,
            max_capacitance,
            pin_order: Arc::new(pin_order),
            pin_info: Arc::new(pin_info),
            delay: Arc::new(delay),
            transition: Arc::new(transition),
            internal_power: Arc::new(internal_power),
            timing_arcs: Arc::new(timing_arcs),
            logic_truth_table: Arc::new(logic_truth_table),
        }
    }

    pub fn check_cell(
        &self,
        slew: HashMap<String, NotNan<f64>>,
        load: NotNan<f64>,
    ) {
        if load > self.max_capacitance {
            panic!("Load {} exceeds max capacitance {}", load, self.max_capacitance);
        }
        for (pin, &(_load, max_slew)) in self.pin_info.iter() {
            if slew[pin] > max_slew {
                panic!("Slew {} exceeds max slew {} for pin {}", slew[pin], max_slew, pin);
            }
        }
    }

    pub fn lookup_table(
        &self,
        mode: &str,
        slews: HashMap<String, NotNan<f64>>,
        load: NotNan<f64>,
    ) -> Result<NotNan<f64>, String> {
        let lut_hash = match mode {
            "delay" => &self.delay,
            "transition" => &self.transition,
            "internal_power" => &self.internal_power,
            _ => panic!("Unknown NLDM table: {}", mode),
        };
        // println!("Looking up NLDM table for mode: {}, slews: {:?}, load: {}\n", mode, slews, load);
        // println!("Available pins in NLDM: {:?}\n", lut_hash);
        let mut final_result = NotNan::new(f64::NEG_INFINITY).unwrap();
        if mode == "internal_power" {
            final_result = NotNan::new(0.0).unwrap(); 
        }
    
        for (pin, slew) in slews.iter() {
            let lut = &lut_hash[pin];
            let index_1 = &lut[0]; // 第一行是 Index_1
            let index_2 = &lut[1]; // 第二行是 Index_2
            let table = &lut[2..]; // 剩下的部分是查找表
    
            // 在 Index_1 中找到 x 的最近邻
            let x_idx = if slew < &index_1[0] {
                1 // 如果小于范围，使用第一个区间进行外推
            } else if slew > &index_1[index_1.len() - 1] {
                index_1.len() - 1 // 如果大于范围，使用最后一个区间进行外推
            } else {
                index_1
                    .iter()
                    .position(|v| *v >= *slew)
                    .unwrap_or(index_1.len() - 1)
                    .max(1) // 确保 x_idx 至少为 1
            };
            let x0 = index_1[x_idx - 1];
            let x1 = index_1[x_idx];
    
            // 在 Index_2 中找到 y 的最近邻
            let y_idx = if load < index_2[0] {
                1 // 如果小于范围，使用第一个区间进行外推
            } else if load > index_2[index_2.len() - 1] {
                index_2.len() - 1 // 如果大于范围，使用最后一个区间进行外推
            } else {
                index_2
                    .iter()
                    .position(|&v| v >= load)
                    .unwrap_or(index_2.len() - 1)
                    .max(1) // 确保 y_idx 至少为 1
            };
            let y0 = index_2[y_idx - 1];
            let y1 = index_2[y_idx];
    
            // 提取四个邻近点的值
            let q00 = table[x_idx - 1][y_idx - 1];
            let q01 = table[x_idx - 1][y_idx];
            let q10 = table[x_idx][y_idx - 1];
            let q11 = table[x_idx][y_idx];
    
            // 计算权重
            let wx = if x1 == x0 {
                NotNan::new(1.0).unwrap() // 避免除以零
            } else {
                (x1 - slew) / (x1 - x0)
            };
            let wy = if y1 == y0 {
                NotNan::new(1.0).unwrap() // 避免除以零
            } else {
                (y1 - load) / (y1 - y0)
            };
    
            // 双线性插值
            let result = q00 * wx * wy
                + q01 * wx * (NotNan::new(1.0).unwrap() - wy)
                + q10 * (NotNan::new(1.0).unwrap() - wx) * wy
                + q11 * (NotNan::new(1.0).unwrap() - wx) * (NotNan::new(1.0).unwrap() - wy);
    
            if mode == "internal_power"{
                final_result += result;
            } else if result > final_result {
                final_result = result;
            }
        }
        if mode == "internal_power"{
            final_result = final_result / NotNan::new(slews.len() as f64).unwrap(); 
        }
    
        Ok(final_result)
    }
}
