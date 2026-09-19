use crate::*;
use ordered_float::NotNan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtendedCost {
    pub components: [NotNan<f64>; 5],
}
pub const INFINITY: ExtendedCost = ExtendedCost { components: [unsafe { NotNan::new_unchecked(std::f64::INFINITY) }; 5] };

impl ExtendedCost {
    pub fn zero() -> Self {
        ExtendedCost {
            components: [NotNan::new(0.0).unwrap(); 5],
        }
    }

    pub fn infinity() -> Self {
        ExtendedCost {
            components: [NotNan::new(std::f64::INFINITY).unwrap(); 5],
        }
    }

    pub fn abs(&self) -> NotNan<f64> {
        let w1: NotNan<f64> = NotNan::new(12.0).unwrap();  // 延迟权重
        w1 * self.components[0].into_inner() * self.components[1].into_inner() * self.components[0].into_inner() * self.components[2].into_inner() 
        // w1 * self.components[0].into_inner() + 
        // w2 * self.components[1].into_inner() + 
        // w3 * self.components[2].into_inner() + 
        // w5 * self.components[4].into_inner()
        // w4 * self.components[3].into_inner()
    }

    // 关键路径上的成本直接相加
    pub fn combine_costs(costs: &[ExtendedCost]) -> ExtendedCost {
        if costs.is_empty() {
            return ExtendedCost::zero();
        }
        costs.iter().fold(ExtendedCost::zero(), |mut acc, &cost| {
            acc += cost;
            acc
        })
    }

    // 结合其余的成本计算总成本
    pub fn critical_path_cost(costs: &[ExtendedCost]) -> ExtendedCost {
        if costs.is_empty() {
            return ExtendedCost::zero();
        }
        
        // 按第一个分量排序找最大值
        let max_cost = costs.iter()
            .max_by(|a, b| a.components[0].partial_cmp(&b.components[0]).unwrap())
            .unwrap();

        // 返回一个新的 Cost，只有第一个分量保留最大值，其他分量正常累加
        ExtendedCost {
            components: [
                max_cost.components[0],
                costs.iter().map(|c| c.components[1]).sum(),
                costs.iter().map(|c| c.components[2]).sum(),
                costs[0].components[3],
                costs[0].components[4],
            ]
        }
    }
}



// 实现转换方法
impl PartialOrd for ExtendedCost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        for i in 0..4 {
            if self.components[i] != other.components[i] {
                return self.components[i].partial_cmp(&other.components[i]);
            }
        }
        Some(std::cmp::Ordering::Equal)
    }
}

impl std::ops::Add for ExtendedCost {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        ExtendedCost {
            components: [
                self.components[0] + other.components[0],
                self.components[1] + other.components[1],
                self.components[2] + other.components[2],
                self.components[3] + other.components[3],
                self.components[4] + other.components[4],
            ]
        }
    }
}

// 添加 AddAssign 实现
impl std::ops::AddAssign for ExtendedCost {
    fn add_assign(&mut self, other: Self) {
        self.components[0] += other.components[0];
        self.components[1] += other.components[1];
        self.components[2] += other.components[2];
        self.components[3] += other.components[3];
        self.components[4] += other.components[4];
    }
}

// 为 Cost 实现 Sum trait
impl std::iter::Sum for ExtendedCost {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(ExtendedCost::zero(), |a, b| a + b)
    }
}

impl std::ops::Sub for ExtendedCost {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        ExtendedCost {
            components: [
                self.components[0] - other.components[0],
                self.components[1] - other.components[1],
                self.components[2] - other.components[2],
                self.components[3] - other.components[3],
                self.components[4] - other.components[4],
            ]
        }
    }
}
