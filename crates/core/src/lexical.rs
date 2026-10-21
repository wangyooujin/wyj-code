//! 本地词法检索：CJK n-gram 分词 + BM25 打分。
//!
//! 刻意不引入 embedding / 向量库——本仓库的检索面（记忆 claim、会话历史）
//! 都以「同一批专有名词反复出现」为主，词法命中的精度足够，且省掉一次远程
//! 调用。三处调用方（`memory_v3::search`、`memory_v3::project_brief` 的
//! `pick_top`、`session_store::search`）此前各写一份等价实现，分词规则一改
//! 就要改三处——这里收敛成唯一一份。

use std::collections::HashMap;

/// 把文本切成可比较的词元。
///
/// - ASCII 片段按 `[alnum-_]` 切，长度 ≥2 才保留（`a`/`x` 这类单字母没有区分度）；
/// - 非 ASCII 走 CJK n-gram（1~3 字窗口），单字层过滤停用字；
/// - 连续 CJK 段 ≤12 字时，额外把整段作为一个 token——中文专名（「招商银行」）
///   整体命中比碎成 n-gram 更可靠。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut non_ascii = Vec::new();
    let flush_ascii = |ascii: &mut String, tokens: &mut Vec<String>| {
        let token = ascii.trim_matches(['-', '_']).to_lowercase();
        if token.chars().count() >= 2 {
            tokens.push(token);
        }
        ascii.clear();
    };
    let flush_non_ascii = |chars: &mut Vec<char>, tokens: &mut Vec<String>| {
        if chars.is_empty() {
            return;
        }
        if chars.len() <= 12 {
            tokens.push(chars.iter().collect::<String>());
        }
        for width in 1..=3 {
            if chars.len() < width {
                continue;
            }
            for window in chars.windows(width) {
                let token = window.iter().collect::<String>();
                if width > 1 || !is_cjk_stop_token(&token) {
                    tokens.push(token);
                }
            }
        }
        chars.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            flush_non_ascii(&mut non_ascii, &mut tokens);
            ascii.push(ch);
        } else if !ch.is_ascii() && !ch.is_whitespace() && !ch.is_ascii_punctuation() {
            flush_ascii(&mut ascii, &mut tokens);
            non_ascii.push(ch);
        } else {
            flush_ascii(&mut ascii, &mut tokens);
            flush_non_ascii(&mut non_ascii, &mut tokens);
        }
    }
    flush_ascii(&mut ascii, &mut tokens);
    flush_non_ascii(&mut non_ascii, &mut tokens);
    tokens
}

fn is_cjk_stop_token(token: &str) -> bool {
    matches!(
        token,
        "的" | "了" | "是" | "我" | "你" | "再" | "一" | "下" | "和"
    )
}

pub fn term_frequencies(text: &str) -> HashMap<String, usize> {
    let mut frequencies = HashMap::new();
    for token in tokenize(text) {
        *frequencies.entry(token).or_insert(0) += 1;
    }
    frequencies
}

/// 一批文档的 BM25 索引。
///
/// 只统计 query 侧用到的 df 会更省，但那样每次查询都要重扫全部文档——
/// `new` 本来就是 O(总词数)，一次算全量 df 反而只需一遍。
pub struct Bm25Scorer {
    doc_terms: Vec<HashMap<String, usize>>,
    doc_lens: Vec<f32>,
    df: HashMap<String, usize>,
    average_len: f32,
    total_docs: f32,
}

impl Bm25Scorer {
    pub fn new(documents: &[String]) -> Self {
        let mut doc_terms = Vec::with_capacity(documents.len());
        let mut doc_lens = Vec::with_capacity(documents.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        for document in documents {
            let terms = term_frequencies(document);
            let len = terms.values().sum::<usize>() as f32;
            for term in terms.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            doc_terms.push(terms);
            doc_lens.push(len);
        }
        let total_len: f32 = doc_lens.iter().sum();
        let average_len = if doc_lens.is_empty() {
            0.0
        } else {
            total_len / doc_lens.len() as f32
        };
        Self {
            doc_terms,
            doc_lens,
            df,
            average_len,
            total_docs: documents.len() as f32,
        }
    }

    pub fn len(&self) -> usize {
        self.doc_terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc_terms.is_empty()
    }

    /// 文档命中任一 query 词元的原始分（不含 confidence / kind / source 这类
    /// 调用方各自的 boost）。
    pub fn score(&self, doc: usize, query_terms: &HashMap<String, usize>) -> f32 {
        let Some(terms) = self.doc_terms.get(doc) else {
            return 0.0;
        };
        let doc_len = self.doc_lens[doc];
        let mut score = 0.0f32;
        for (term, query_count) in query_terms {
            let tf = *terms.get(term).unwrap_or(&0) as f32;
            if tf == 0.0 {
                continue;
            }
            let df = *self.df.get(term).unwrap_or(&0) as f32;
            let idf = ((self.total_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
            let denominator = tf + 1.2 * (0.25 + 0.75 * doc_len / self.average_len.max(1.0));
            score += idf * (tf * 2.2 / denominator) * (*query_count as f32).sqrt();
        }
        score
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_runs_become_whole_tokens_and_ngrams() {
        let tokens = tokenize("招商银行持仓");
        assert!(
            tokens.contains(&"招商银行持仓".to_string()),
            "整段 ≤12 字要保留"
        );
        assert!(tokens.contains(&"招商".to_string()));
        assert!(tokens.contains(&"持仓".to_string()));
    }

    #[test]
    fn ascii_short_tokens_are_dropped() {
        let tokens = tokenize("a bc def_gh");
        assert!(!tokens.contains(&"a".to_string()));
        assert!(tokens.contains(&"bc".to_string()));
        assert!(tokens.contains(&"def_gh".to_string()));
    }

    #[test]
    fn bm25_ranks_the_document_that_contains_the_query_higher() {
        let documents = vec![
            "memory v3 stores claims in json".to_string(),
            "unrelated text about rendering widgets".to_string(),
        ];
        let scorer = Bm25Scorer::new(&documents);
        let query = term_frequencies("claims");
        assert!(scorer.score(0, &query) > scorer.score(1, &query));
    }

    #[test]
    fn bm25_handles_empty_corpus() {
        let scorer = Bm25Scorer::new(&[]);
        assert!(scorer.is_empty());
        assert_eq!(scorer.score(0, &term_frequencies("x")), 0.0);
    }
}
