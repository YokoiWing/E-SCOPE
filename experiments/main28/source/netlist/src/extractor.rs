// 导入提取器库和索引映射库
use extraction_gym::extract::*; // 提取器框架库
use indexmap::IndexMap; // 保持插入顺序的哈希映射

/// 定义提取器的最优性级别枚举
#[derive(PartialEq, Eq)] // 自动实现部分相等和完全相等 trait
pub enum Optimal {
    Tree,    // 提取器对树结构是最优的
    DAG,     // 提取器对DAG(有向无环图)结构是最优的
    Neither, // 提取器不是最优的(启发式方法)
}

/// 提取器详细信息结构体
pub struct ExtractorDetail {
    extractor: Box<dyn Extractor>, // 提取器实现的动态分发指针
    optimal: Optimal,              // 该提取器的最优性级别
    use_for_bench: bool,           // 是否用于基准测试
}

// 为 ExtractorDetail 实现方法
impl ExtractorDetail {
    // 获取提取器实现的引用
    pub fn get_extractor(&self) -> &dyn Extractor {
        &*self.extractor // 解引用并返回提取器 trait 对象
    }

    // 获取最优性级别
    pub fn get_optimal(&self) -> &Optimal {
        &self.optimal // 返回最优性级别的引用
    }

    // 获取是否用于基准测试的标志
    pub fn get_use_for_bench(&self) -> bool {
        self.use_for_bench // 返回布尔值
    }
}

/// 创建并返回所有提取器的注册表
pub fn extractors() -> IndexMap<&'static str, ExtractorDetail> {
    // 创建提取器数组并转换为索引映射
    let extractors: IndexMap<&'static str, ExtractorDetail> = [
        (
            "iterative-greedy-dag",
            ExtractorDetail {
                extractor: iterative_greedy_dag::IterativeGreedyDagExtractor.boxed(),
                optimal: Optimal::Tree,
                use_for_bench: true,
            },
        ),
        (
            "iterative-greedy-dag-SA",
            ExtractorDetail {
                extractor: iterative_greedy_dag_SA::IterativeGreedyDagSaExtractor::default()
                    .boxed(),
                optimal: Optimal::Tree,
                use_for_bench: true,
            },
        ),
        (
            "timing-pareto-beam",
            ExtractorDetail {
                extractor: timing_pareto_beam::TimingParetoBeamExtractor::default().boxed(),
                optimal: Optimal::Neither,
                use_for_bench: true,
            },
        ),
        // 底部向上提取器
        (
            "bottom-up", // 提取器名称
            ExtractorDetail {
                extractor: bottom_up::BottomUpExtractor.boxed(), // 装箱的提取器实现
                optimal: Optimal::Tree,                          // 对树结构最优
                use_for_bench: true,                             // 用于基准测试
            },
        ),
        // 更快的底部向上提取器
        (
            "faster-bottom-up",
            ExtractorDetail {
                extractor: faster_bottom_up::FasterBottomUpExtractor.boxed(),
                optimal: Optimal::Tree,
                use_for_bench: true,
            },
        ),
        // 优先级队列提取器
        (
            "prio-queue",
            ExtractorDetail {
                extractor: prio_queue::PrioQueueExtractor.boxed(),
                optimal: Optimal::Tree,
                use_for_bench: true,
            },
        ),
        // 更快的贪婪DAG提取器
        (
            "faster-greedy-dag",
            ExtractorDetail {
                extractor: faster_greedy_dag::FasterGreedyDagExtractor.boxed(),
                optimal: Optimal::Neither, // 不是最优的(启发式方法)
                use_for_bench: true,
            },
        ),
        /* 注释掉的全局贪婪DAG提取器
        (
            "global-greedy-dag",
            ExtractorDetail {
                extractor: global_greedy_dag::GlobalGreedyDagExtractor.boxed(),
                optimal: Optimal::Neither,
                use_for_bench: true,
            },
        ),*/
        #[cfg(feature = "ilp-cbc")]
        (
            "nldm-oracle-ilp",
            ExtractorDetail {
                extractor: nldm_oracle_ilp::NldmOracleCbcExtractor::default().boxed(),
                optimal: Optimal::Neither,
                use_for_bench: true,
            },
        ),
    ]
    .into_iter() // 将数组转换为迭代器
    .collect(); // 收集迭代器中的元素到IndexMap

    return extractors; // 返回构建的提取器映射
}
