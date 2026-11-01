//! 이미 스캔해 둔 트리에서 항목을 찾는다: 이름이 일치하는 항목, 또는 가장 큰 항목.
//!
//! 디스크를 다시 읽지 않고 메모리의 트리만 훑는다. 그래서 결과는 "마지막으로 스캔한 시점"의
//! 사실이고, 얼마나 오래된 데이터인지는 헤더가 따로 보여 준다.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::scanner::Node;

/// 어떤 종류의 항목을 찾을지
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    All,
    Files,
    Dirs,
}

impl Kind {
    pub fn next(self) -> Self {
        match self {
            Kind::All => Kind::Files,
            Kind::Files => Kind::Dirs,
            Kind::Dirs => Kind::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::All => "파일+폴더",
            Kind::Files => "파일만",
            Kind::Dirs => "폴더만",
        }
    }

    fn accepts(self, node: &Node) -> bool {
        match self {
            Kind::All => true,
            Kind::Files => !node.is_dir,
            Kind::Dirs => node.is_dir,
        }
    }
}

/// 찾은 항목 하나
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub size: u64,
    pub is_dir: bool,
    /// 이미 찾은 폴더 안에 들어 있어서 합계에는 넣지 않은 항목인가
    pub nested: bool,
    /// 찾기 시작한 폴더에서 이 항목까지 내려가는 자식 인덱스들
    pub path: Vec<u32>,
}

// 큰 것이 "더 좋은" 순서. 크기가 같으면 경로가 앞선 것이 더 좋다. (결과가 늘 같은 순서가 되도록)
impl Ord for Hit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.size
            .cmp(&other.size)
            .then_with(|| other.path.cmp(&self.path))
    }
}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// 찾기 결과
#[derive(Debug, Default)]
pub struct Found {
    /// 큰 순서로, 많아도 `limit`건
    pub hits: Vec<Hit>,
    /// 일치한 항목의 전체 건수 (`hits`보다 많을 수 있다)
    pub count: usize,
    /// 합계. 일치한 폴더 안에 또 일치한 항목이 있으면 안쪽은 더하지 않는다.
    pub total: u64,
    /// 합계에서 뺀(다른 일치 항목 안에 들어 있는) 건수
    pub nested: usize,
}

/// `root` 아래(`root` 자신은 제외)에서 이름에 `query`가 들어 있는(대소문자 무시) `kind` 항목을 찾는다.
/// `query`가 비어 있으면 `kind` 항목 전부가 대상이라서 가장 큰 항목 순위가 된다.
pub fn find(root: &Node, query: &str, kind: Kind, limit: usize) -> Found {
    let mut walk = Walk {
        needle: Needle::new(query),
        kind,
        limit,
        heap: BinaryHeap::new(),
        count: 0,
        total: 0,
        nested: 0,
        path: Vec::new(),
    };
    walk.visit(root, false);

    let mut hits: Vec<Hit> = walk.heap.into_iter().map(|Reverse(h)| h).collect();
    hits.sort_by(|a, b| b.cmp(a));
    Found {
        hits,
        count: walk.count,
        total: walk.total,
        nested: walk.nested,
    }
}

struct Walk {
    needle: Needle,
    kind: Kind,
    limit: usize,
    /// 지금까지 본 것 중 가장 좋은 `limit`건. 가장 나쁜 것이 맨 위에 있어 밀려날 차례를 바로 안다.
    heap: BinaryHeap<Reverse<Hit>>,
    count: usize,
    total: u64,
    nested: usize,
    path: Vec<u32>,
}

impl Walk {
    fn visit(&mut self, node: &Node, inside_match: bool) {
        for (i, child) in node.children.iter().enumerate() {
            self.path.push(i as u32);
            let matched = self.kind.accepts(child) && self.needle.is_in(&child.name);
            if matched {
                self.count += 1;
                if inside_match {
                    self.nested += 1;
                } else {
                    self.total += child.size;
                }
                self.offer(child, inside_match);
            }
            self.visit(child, inside_match || matched);
            self.path.pop();
        }
    }

    fn offer(&mut self, node: &Node, nested: bool) {
        if self.limit == 0 {
            return;
        }
        if self.heap.len() >= self.limit {
            let Some(Reverse(worst)) = self.heap.peek() else { return };
            // 경로를 복사하기 전에, 밀어낼 만큼 좋은 항목인지부터 본다
            let better = node.size > worst.size
                || (node.size == worst.size && self.path.as_slice() < worst.path.as_slice());
            if !better {
                return;
            }
            self.heap.pop();
        }
        self.heap.push(Reverse(Hit {
            size: node.size,
            is_dir: node.is_dir,
            nested,
            path: self.path.clone(),
        }));
    }
}

/// 대소문자를 가리지 않는 부분 문자열 검사. 이름이 수백만 개라서 ASCII끼리는 새로 할당하지 않는다.
struct Needle {
    lower: String,
    ascii: bool,
}

impl Needle {
    fn new(query: &str) -> Self {
        Needle {
            lower: query.to_lowercase(),
            ascii: query.is_ascii(),
        }
    }

    fn is_in(&self, name: &str) -> bool {
        if self.lower.is_empty() {
            return true;
        }
        if self.ascii && name.is_ascii() {
            let (n, h) = (self.lower.as_bytes(), name.as_bytes());
            return h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
        }
        name.to_lowercase().contains(&self.lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> Node {
        Node {
            name: name.into(),
            is_dir: false,
            size,
            unreadable: 0,
            scanned: 1,
            oldest: 1,
            children: Box::new([]),
        }
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        Node {
            name: name.into(),
            is_dir: true,
            size: children.iter().map(|c| c.size).sum(),
            unreadable: 0,
            scanned: 1,
            oldest: 1,
            children: children.into_boxed_slice(),
        }
    }

    /// root
    /// ├─ app/            (node_modules 두 겹)
    /// │   ├─ node_modules/   [a.js 100, node_modules/ [b.js 40]]
    /// │   └─ main.rs 10
    /// ├─ Docs/ [Node_Modules/ [c.js 7], readme.md 3]
    /// └─ big.bin 500
    fn sample() -> Node {
        dir(
            "",
            vec![
                dir(
                    "app",
                    vec![
                        dir(
                            "node_modules",
                            vec![file("a.js", 100), dir("node_modules", vec![file("b.js", 40)])],
                        ),
                        file("main.rs", 10),
                    ],
                ),
                dir("Docs", vec![dir("Node_Modules", vec![file("c.js", 7)]), file("readme.md", 3)]),
                file("big.bin", 500),
            ],
        )
    }

    fn names_of(root: &Node, hit: &Hit) -> String {
        let mut node = root;
        let mut out = Vec::new();
        for &i in &hit.path {
            node = &node.children[i as usize];
            out.push(&*node.name);
        }
        out.join("/")
    }

    #[test]
    fn an_empty_query_ranks_the_biggest_files() {
        let root = sample();
        let found = find(&root, "", Kind::Files, 3);
        let top: Vec<_> = found.hits.iter().map(|h| (names_of(&root, h), h.size)).collect();
        assert_eq!(
            top,
            [
                ("big.bin".to_string(), 500),
                ("app/node_modules/a.js".to_string(), 100),
                ("app/node_modules/node_modules/b.js".to_string(), 40),
            ]
        );
        assert_eq!(found.count, 6, "all six files count, not just the ones shown");
        assert_eq!(found.total, 660);
        assert_eq!(found.nested, 0, "files never contain each other");
    }

    #[test]
    fn name_matches_ignore_case_and_match_part_of_a_name() {
        let root = sample();
        let found = find(&root, "node_mod", Kind::Dirs, 10);
        let names: Vec<_> = found.hits.iter().map(|h| names_of(&root, h)).collect();
        assert_eq!(
            names,
            ["app/node_modules", "app/node_modules/node_modules", "Docs/Node_Modules"]
        );
    }

    #[test]
    fn the_total_does_not_count_a_match_inside_another_match_twice() {
        let root = sample();
        let found = find(&root, "node_modules", Kind::Dirs, 10);
        // app/node_modules = 140, 그 안의 node_modules = 40(중복), Docs/Node_Modules = 7
        assert_eq!(found.count, 3);
        assert_eq!(found.total, 147, "140 + 7, not 140 + 40 + 7");
        assert_eq!(found.nested, 1);
        let inner = found.hits.iter().find(|h| h.nested).unwrap();
        assert_eq!(names_of(&root, inner), "app/node_modules/node_modules");
        assert!(found.hits.iter().filter(|h| !h.nested).count() == 2);
    }

    #[test]
    fn the_limit_keeps_the_biggest_but_the_counts_cover_everything() {
        let root = sample();
        let found = find(&root, "", Kind::All, 2);
        assert_eq!(found.hits.len(), 2);
        assert!(found.hits[0].size >= found.hits[1].size);
        assert_eq!(found.hits[0].size, 500, "the root's biggest child is big.bin");
        assert_eq!(found.count, 11, "every entry under the root");
        assert_eq!(find(&root, "", Kind::All, 0).hits.len(), 0);
    }

    #[test]
    fn equal_sizes_come_out_in_a_stable_order() {
        let root = dir("", vec![file("b", 5), file("a", 5), file("c", 5), file("d", 1)]);
        let found = find(&root, "", Kind::Files, 3);
        let order: Vec<_> = found.hits.iter().map(|h| h.path[0]).collect();
        assert_eq!(order, [0, 1, 2], "ties keep the tree order, and the limit drops the same ones every time");
    }

    #[test]
    fn non_ascii_names_match_too() {
        let root = dir("", vec![file("한글문서.txt", 9), file("Résumé.pdf", 8), file("other", 1)]);
        assert_eq!(find(&root, "문서", Kind::Files, 10).count, 1);
        assert_eq!(find(&root, "RÉSUMÉ", Kind::Files, 10).count, 1);
        assert_eq!(find(&root, "pdf", Kind::Files, 10).count, 1);
    }
}
