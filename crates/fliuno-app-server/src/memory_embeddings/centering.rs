use super::MemoryEmbeddingProvider;

const NEUTRAL: [[&str; 6]; 2] = [
    [
        "Complete this task as requested.",
        "What should we do next?",
        "Can you help me with this?",
        "Here is a detail about my preferences.",
        "Please keep this information for later.",
        "This is useful in my work.",
    ],
    [
        "按要求处理好这项任务。",
        "接下来应该做什么？",
        "能帮我处理这件事吗？",
        "这里有一条关于我的偏好。",
        "请把这个信息留到以后。",
        "它在我的工作中很有用。",
    ],
];

pub(super) enum Role {
    Query,
    Passage,
}

pub(super) struct Centers {
    query: [Vec<f64>; 2],
    passage: [Vec<f64>; 2],
}

fn norm(values: &[f32]) -> f64 {
    values
        .iter()
        .map(|&v| f64::from(v).powi(2))
        .sum::<f64>()
        .sqrt()
}

impl Centers {
    pub(super) async fn load(provider: &MemoryEmbeddingProvider) -> Option<Self> {
        async fn means(provider: &MemoryEmbeddingProvider, prefix: &str) -> Option<[Vec<f64>; 2]> {
            let texts: Vec<_> = NEUTRAL
                .iter()
                .flatten()
                .map(|text| format!("{prefix}{text}"))
                .collect();
            let rows = provider.embed(&texts).await?;
            let mut means = std::array::from_fn(|_| vec![0.0; provider.config.dimensions]);
            for (rows, mean) in rows
                .as_chunks::<{ NEUTRAL[0].len() }>()
                .0
                .iter()
                .zip(&mut means)
            {
                for vector in rows {
                    let scale = norm(vector) * rows.len() as f64;
                    for (result, value) in mean.iter_mut().zip(vector) {
                        *result += f64::from(*value) / scale;
                    }
                }
            }
            Some(means)
        }
        let (query, passage) =
            tokio::join!(means(provider, "query: "), means(provider, "passage: "));
        Some(Self {
            query: query?,
            passage: passage?,
        })
    }

    pub(super) fn transform(&self, values: &mut [f32], text: &str, role: Role) -> bool {
        let contains_han = text.chars().any(|c| matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{20000}'..='\u{323af}'));
        let means = match role {
            Role::Query => &self.query,
            Role::Passage => &self.passage,
        };
        let scale = norm(values);
        for (value, mean) in values.iter_mut().zip(&means[usize::from(contains_han)]) {
            *value = (f64::from(*value) / scale - mean) as f32;
        }
        // An embedding equal to its neutral center has no relevance signal.
        norm(values) > 1e-6
    }
}
