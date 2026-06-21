//! 跨会话编排:维护任务队列,把队列任务投给空闲会话。
//!
//! 队列来源优先级:有 `queue_file` 用文件(每行一个任务),否则用 config 内联
//! `task_queue`。出队后若有 queue_file 则改写文件。默认禁用,需 `enabled = true`。

use crate::config::{expand_tilde, Orchestration};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// 一次派发动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    pub session: String,
    pub task: String,
}

/// 任务队列:从文件或内联配置加载,可出队并持久化。
pub struct TaskQueue {
    tasks: Vec<String>,
    file: Option<PathBuf>,
}

impl TaskQueue {
    /// 按配置加载队列。
    pub fn load(orch: &Orchestration) -> Result<TaskQueue> {
        match &orch.queue_file {
            Some(f) => {
                let path = expand_tilde(f);
                let tasks = if path.exists() {
                    let text = std::fs::read_to_string(&path)
                        .with_context(|| format!("读取队列文件失败: {}", path.display()))?;
                    parse_tasks(&text)
                } else {
                    // 文件不存在:用内联队列做种子。
                    orch.task_queue.clone()
                };
                Ok(TaskQueue {
                    tasks,
                    file: Some(path),
                })
            }
            None => Ok(TaskQueue {
                tasks: orch.task_queue.clone(),
                file: None,
            }),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// 弹出队首任务(不持久化;持久化用 `persist`)。
    pub fn pop_front(&mut self) -> Option<String> {
        if self.tasks.is_empty() {
            None
        } else {
            Some(self.tasks.remove(0))
        }
    }

    /// 把当前队列写回文件(无 queue_file 则空操作)。
    pub fn persist(&self) -> Result<()> {
        if let Some(path) = &self.file {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let text = self.tasks.join("\n");
            let body = if text.is_empty() {
                String::new()
            } else {
                format!("{}\n", text)
            };
            std::fs::write(path, body)
                .with_context(|| format!("写队列文件失败: {}", path.display()))?;
        }
        Ok(())
    }
}

/// 解析队列文本:每行一个任务,跳过空行和 `#` 注释。
fn parse_tasks(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orch_inline(tasks: &[&str]) -> Orchestration {
        Orchestration {
            enabled: true,
            session_match: None,
            task_queue: tasks.iter().map(|s| s.to_string()).collect(),
            queue_file: None,
        }
    }

    #[test]
    fn inline_queue_pops_in_order() {
        let mut q = TaskQueue::load(&orch_inline(&["task one", "task two"])).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front().as_deref(), Some("task one"));
        assert_eq!(q.pop_front().as_deref(), Some("task two"));
        assert_eq!(q.pop_front(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn parse_skips_blank_and_comments() {
        let tasks = parse_tasks("first\n\n# a comment\n  second  \n");
        assert_eq!(tasks, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn file_queue_persists_after_pop() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ccwatch_queue_test_{}.txt", std::process::id()));
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let orch = Orchestration {
            enabled: true,
            session_match: None,
            task_queue: Vec::new(),
            queue_file: Some(path.to_string_lossy().to_string()),
        };

        let mut q = TaskQueue::load(&orch).unwrap();
        assert_eq!(q.pop_front().as_deref(), Some("alpha"));
        q.persist().unwrap();

        // 重新加载应只剩 beta、gamma。
        let q2 = TaskQueue::load(&orch).unwrap();
        assert_eq!(q2.len(), 2);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_seeds_from_inline() {
        let path = std::env::temp_dir().join("ccwatch_no_such_queue_xyz.txt");
        std::fs::remove_file(&path).ok();
        let orch = Orchestration {
            enabled: true,
            session_match: None,
            task_queue: vec!["seed".to_string()],
            queue_file: Some(path.to_string_lossy().to_string()),
        };
        let q = TaskQueue::load(&orch).unwrap();
        assert_eq!(q.len(), 1);
    }
}
