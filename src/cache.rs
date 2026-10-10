// what a prefix cache would have done with a chat request: how much of its prompt some earlier
// request had already put through the model, and where the two part ways.
//
// A SERVER LIKE llama.cpp KEEPS THE STATE OF A PROMPT IT HAS EVALUATED and reuses the longest
// prefix a new prompt shares with it; whatever follows the first difference is evaluated again.
// a client that reorders its tools between two turns of one conversation, or rewrites an early
// message, therefore pays for the whole conversation again -- and nothing in either request says
// so. this is that comparison, made here so a test can read it off a capture.
//
// THE PROMPT IS RENDERED TOOLS FIRST, THEN THE MESSAGES IN ORDER, as the chat templates that
// carry tools do (qwen, hermes): the tools sit in or beside the system prompt, ahead of the
// conversation. the rendering is this file's own and not any model's, so `cached_chars` is a
// measure of SHARED PREFIX and not a token count.
//
// AGAINST EVERY EARLIER REQUEST, not only the last: a server holds more than one prompt's state,
// and a title request slipped between two turns must not read as the conversation losing its
// cache.

use serde_json::{json, Value};

// one request, as the pieces a prompt is rendered from.
#[derive(Clone)]
pub struct Prompt {
    tools: Vec<String>,
    messages: Vec<String>,
}

impl Prompt {
    pub fn of(body: &Value) -> Prompt {
        let rows = |key: &str| -> Vec<String> {
            let list = body.get(key).and_then(Value::as_array);
            list.map(|l| l.iter().map(Value::to_string).collect())
                .unwrap_or_default()
        };
        Prompt {
            tools: rows("tools"),
            messages: rows("messages"),
        }
    }

    fn pieces(&self) -> impl Iterator<Item = &String> {
        self.tools.iter().chain(&self.messages)
    }

    fn chars(&self) -> usize {
        self.pieces().map(|p| p.chars().count() + 1).sum()
    }

    // how many characters of this prompt `earlier` shares from the start, and the first piece
    // that differs: ("tools" | "messages", its index), or none when this prompt only adds to it.
    fn shared(&self, earlier: &Prompt) -> (usize, Option<(&'static str, usize)>) {
        let mut chars = 0;
        let mut theirs = earlier.pieces();
        for (i, mine) in self.pieces().enumerate() {
            match theirs.next() {
                Some(t) if t == mine => chars += mine.chars().count() + 1,
                Some(t) => {
                    let same = mine.chars().zip(t.chars()).take_while(|(a, b)| a == b);
                    return (chars + same.count(), Some(self.place(i)));
                }
                None => break,
            }
        }
        (chars, None)
    }

    fn place(&self, piece: usize) -> (&'static str, usize) {
        match piece < self.tools.len() {
            true => ("tools", piece),
            false => ("messages", piece - self.tools.len()),
        }
    }
}

// what a capture says about `prompt` against everything seen before it.
pub fn report(prompt: &Prompt, seen: &[Prompt]) -> Value {
    let best = seen
        .iter()
        .map(|earlier| prompt.shared(earlier))
        .max_by_key(|(chars, _)| *chars);
    let (cached, diverged) = best.unwrap_or((0, None));
    json!({
        "prompt_chars": prompt.chars(),
        "cached_chars": cached,
        // where the best earlier prompt stops matching: null when this one only extends it,
        // and for the first request of all.
        "diverged": diverged.map(|(what, index)| json!({"in": what, "index": index})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(tools: &[&str], messages: &[&str]) -> Prompt {
        Prompt::of(&json!({
            "tools": tools.iter().map(|t| json!({"name": t})).collect::<Vec<_>>(),
            "messages": messages.iter().map(|m| json!({"content": m})).collect::<Vec<_>>(),
        }))
    }

    #[test]
    fn a_turn_that_only_adds_to_the_last_one_is_cached_up_to_what_it_adds() {
        let first = prompt(&["a", "b"], &["sys", "hi"]);
        let next = prompt(&["a", "b"], &["sys", "hi", "hello", "more"]);
        let said = report(&next, &[first.clone()]);
        assert_eq!(said["cached_chars"], first.chars());
        assert_eq!(said["diverged"], Value::Null);
        assert!(said["prompt_chars"].as_u64() > said["cached_chars"].as_u64());
    }

    #[test]
    fn tools_in_another_order_cost_the_whole_conversation() {
        let first = prompt(&["a", "b"], &["sys", "hi"]);
        let next = prompt(&["b", "a"], &["sys", "hi", "hello"]);
        let said = report(&next, &[first]);
        assert_eq!(said["diverged"], json!({"in": "tools", "index": 0}));
        // the two tools open alike (`{"name":"`) and then differ: nothing past that is shared.
        assert_eq!(said["cached_chars"], r#"{"name":""#.chars().count());
    }

    #[test]
    fn a_rewritten_message_is_named_and_the_best_earlier_prompt_is_the_one_compared() {
        let other = prompt(&["z"], &["another conversation"]);
        let first = prompt(&["a"], &["sys", "hi"]);
        let next = prompt(&["a"], &["sys", "HI", "hello"]);
        let said = report(&next, &[other, first]);
        assert_eq!(said["diverged"], json!({"in": "messages", "index": 1}));
        assert_eq!(report(&next, &[])["cached_chars"], 0);
    }
}
