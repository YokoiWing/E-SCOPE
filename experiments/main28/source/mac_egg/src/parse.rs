use std::fs;
use std::collections::HashMap;
use anyhow::{anyhow, Context, Result};

/// 解析AIGER文件并转换为逻辑表达式字符串(egg兼容格式)
/// 对于多输出，返回多个表达式，用换行分隔
/// 示例: "(and (not a0) (and a1 a2))\n(and a3 a4)"
pub fn parse_aiger_to_expr(path: &str) -> Result<String> {
    // 读取文件内容（自动关闭文件句柄）
    let content = fs::read_to_string(path)
        .with_context(|| format!("无法读取AIGER文件: {}", path))?;

    let mut lines = content.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty());

    // 解析头部: aag M I L O A
    let header = lines.next()
        .ok_or_else(|| anyhow!("空文件"))?;
    
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() < 6 || parts[0] != "aag" {
        return Err(anyhow!("无效头部，应为'aag M I L O A'"));
    }

    // 解析头部数字字段
    let max_index: usize = parts[1].parse().context("无效M值")?;
    let num_inputs: usize = parts[2].parse().context("无效I值(输入数量)")?;
    let num_latches: usize = parts[3].parse().context("无效L值(锁存器)")?;
    let num_outputs: usize = parts[4].parse().context("无效O值(输出数量)")?;
    let num_ands: usize = parts[5].parse().context("无效A值(AND门数量)")?;

    // 存储变量索引->表达式的映射
    let mut idx_to_expr = HashMap::new();
    
    // 添加常量值 
    idx_to_expr.insert(0, "false".to_string());
    idx_to_expr.insert(1, "true".to_string());

    // ========================================================================
    // 1. 解析输入部分（无'i'前缀！）
    // ========================================================================
    for i in 0..num_inputs {
        let line = lines.next()
            .ok_or_else(|| anyhow!("缺少输入行{}", i))?;
        
        // 输入行应为纯数字（例如："2"）
        let input_idx: usize = line.parse()
            .with_context(|| format!("输入行不是数字: '{}'", line))?;
        
        // 输入变量命名(a0, a1,...) 
        let var_name = format!("a{}", i);
        idx_to_expr.insert(input_idx, var_name);
    }

    // ========================================================================
    // 2. 解析锁存器部分 (L行)
    // ========================================================================
    for i in 0..num_latches {
        let line = lines.next()
            .ok_or_else(|| anyhow!("缺少锁存器行"))?;
        
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 2 {
            return Err(anyhow!("锁存器行需包含2个数字,实际: '{}'", line));
        }
        
        let latch_idx: usize = parts[0].parse().context("无效锁存器索引")?;
        let _next_idx: usize = parts[1].parse().context("无效下一状态索引")?;
        
        // 将锁存器当前状态视为变量 (l0, l1, ...)
        let var_name = format!("l{}", i);
        idx_to_expr.insert(latch_idx, var_name);
        // 注意: 如果需要下一状态表达式，可以额外构建 build_expr(_next_idx, ...)
    }

    // ========================================================================
    // 3. 解析输出部分（无'o'前缀！）
    // ========================================================================
    let mut outputs = Vec::new();
    for _ in 0..num_outputs {
        let line = lines.next()
            .ok_or_else(|| anyhow!("缺少输出行"))?;
        
        // 输出行应为纯数字（例如："6"或"7"）
        let output_idx: usize = line.parse()
            .with_context(|| format!("输出行不是数字: '{}'", line))?;
        outputs.push(output_idx);
    }

    // ========================================================================
    // 4. 解析AND门定义
    // ========================================================================
    for _ in 0..num_ands {
        let line = lines.next()
            .ok_or_else(|| anyhow!("缺少AND门定义行"))?;
        
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(anyhow!("AND行需包含3个数字，实际: '{}'", line));
        }
        
        // 解析三个索引值
        let out_idx: usize = parts[0].parse().context("无效输出索引")?;
        let in1_idx: usize = parts[1].parse().context("无效输入1索引")?;
        let in2_idx: usize = parts[2].parse().context("无效输入2索引")?;
        
        // 构建输入表达式（递归处理取反）
        let expr1 = build_expr(in1_idx, &idx_to_expr)?;
        let expr2 = build_expr(in2_idx, &idx_to_expr)?;
        
        // 创建AND表达式
        let and_expr = format!("(and {} {})", expr1, expr2);
        idx_to_expr.insert(out_idx, and_expr);
    }

    // ========================================================================
    // 5. 构建所有输出对应的表达式，并用换行符连接返回
    // ========================================================================
    let mut output_exprs = Vec::new();
    for &idx in &outputs {
        let expr = build_expr(idx, &idx_to_expr)?;
        output_exprs.push(expr);
    }

    if output_exprs.is_empty() {
        return Err(anyhow!("无有效输出"));
    }

    Ok(output_exprs.join("\n"))
}

/// 递归构建表达式（处理取反和变量引用）
fn build_expr(idx: usize, idx_to_expr: &HashMap<usize, String>) -> Result<String> {
    match idx {
        // 处理常量 
        0 => Ok("false".to_string()),
        1 => Ok("true".to_string()),
        _ => {
            // 奇偶索引处理（奇数=取反）
            let is_negated = (idx % 2) == 1;
            let base_idx = if is_negated { idx - 1 } else { idx };

            // 获取基础表达式
            let base_expr = idx_to_expr.get(&base_idx)
                .cloned()
                .ok_or_else(|| anyhow!("未定义索引: {}", base_idx))?;

            // 应用取反操作
            if is_negated {
                Ok(format!("(not {})", base_expr))
            } else {
                Ok(base_expr)
            }
        }
    }
}