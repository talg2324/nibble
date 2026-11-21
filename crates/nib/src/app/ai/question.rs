//! Answering the questions an agent asks (`AskUserQuestion`): what is picked for each, with the
//! number keys, the arrows or the mouse, or said in the user's own words.

use nib_proto::AiQuestion;

#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Form {
    /// The question the keys work on.
    pub q: usize,
    /// The option the arrows are on in it: its preview shows, Space / Enter pick it.
    pub lit: usize,
    /// What is picked, per question: option numbers (0-based), in order.
    pub picks: Vec<Vec<usize>>,
    /// Answers in the user's own words, per question.
    pub own: Vec<Option<String>>,
}

impl Form {
    pub fn new(questions: &[AiQuestion]) -> Self {
        Self { q: 0, lit: 0, picks: vec![vec![]; questions.len()], own: vec![None; questions.len()] }
    }

    pub fn answered(&self, q: usize) -> bool {
        self.picks.get(q).is_some_and(|p| !p.is_empty()) || self.own.get(q).is_some_and(Option::is_some)
    }

    /// Every question has an answer: it can go.
    pub fn complete(&self) -> bool {
        (0..self.picks.len()).all(|q| self.answered(q))
    }

    /// Pick option `o` of question `q`: it replaces the answer, or (any number may be picked) is
    /// added or taken back.
    pub fn pick(&mut self, questions: &[AiQuestion], q: usize, o: usize) {
        let Some(question) = questions.get(q).filter(|x| o < x.options.len()) else { return };
        self.q = q;
        self.lit = o;
        let picks = &mut self.picks[q];
        if !question.multi {
            *picks = vec![o];
            self.own[q] = None;
        } else if let Some(at) = picks.iter().position(|&p| p == o) {
            picks.remove(at);
        } else {
            picks.push(o);
            picks.sort_unstable();
        }
    }

    /// The user's own words for the question the keys are on: instead of a pick, or (any number
    /// may be picked) with them.
    pub fn say(&mut self, questions: &[AiQuestion], text: String) {
        let q = self.q;
        let Some(question) = questions.get(q) else { return };
        if !question.multi {
            self.picks[q].clear();
        }
        self.own[q] = Some(text);
    }

    /// Put the keys on question `q`, on what is picked in it.
    pub fn focus(&mut self, q: usize) {
        if q < self.picks.len() {
            self.q = q;
            self.lit = self.picks[q].first().copied().unwrap_or(0);
        }
    }

    /// On to the next question without an answer, after this one (round to the start).
    /// `false` when there is none.
    pub fn advance(&mut self) -> bool {
        let n = self.picks.len();
        let next = (1..n).map(|d| (self.q + d) % n).find(|&q| !self.answered(q));
        next.inspect(|&q| self.focus(q)).is_some()
    }

    /// Move the arrows' option up (`-1`) or down (`1`).
    pub fn step(&mut self, questions: &[AiQuestion], d: isize) {
        let n = questions.get(self.q).map_or(0, |x| x.options.len());
        if n > 0 {
            self.lit = self.lit.saturating_add_signed(d).min(n - 1);
        }
    }

    /// The answers, one per question, the way the harness takes them: the picked labels and
    /// the user's words, joined by ", ".
    pub fn answers(&self, questions: &[AiQuestion]) -> Vec<String> {
        questions
            .iter()
            .enumerate()
            .map(|(q, x)| {
                let picked = self.picks[q].iter().filter_map(|&o| x.options.get(o)).map(|o| o.label.as_str());
                picked.chain(self.own[q].as_deref()).collect::<Vec<_>>().join(", ")
            })
            .collect()
    }
}

/// Answers given from outside (`nib ctl ai answer`): an option's number (1-based) stands for
/// its label; anything else is said as it is.
pub(super) fn resolve(questions: &[AiQuestion], given: &[String]) -> Vec<String> {
    questions
        .iter()
        .zip(given)
        .map(|(q, a)| {
            let label = a.trim().parse::<usize>().ok().and_then(|n| q.options.get(n.checked_sub(1)?));
            label.map_or_else(|| a.clone(), |o| o.label.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib_proto::AiOption;

    fn question(text: &str, labels: &[&str], multi: bool) -> AiQuestion {
        let options = labels.iter().map(|l| AiOption { label: (*l).into(), description: String::new(), preview: None }).collect();
        AiQuestion { header: String::new(), question: text.into(), options, multi }
    }

    #[test]
    fn picks_and_words_make_the_answers() {
        let qs = [question("Which?", &["A", "B"], false), question("Also?", &["X", "Y", "Z"], true)];
        let mut f = Form::new(&qs);
        assert!(!f.complete());
        f.pick(&qs, 0, 0);
        f.pick(&qs, 0, 1);
        assert_eq!(f.picks[0], [1], "one of a single choice");
        assert!(f.advance() && f.q == 1, "on to the unanswered one");
        f.pick(&qs, 1, 2);
        f.pick(&qs, 1, 0);
        f.pick(&qs, 1, 2);
        f.pick(&qs, 1, 1);
        assert_eq!(f.picks[1], [0, 1], "a multiple choice toggles, in order");
        f.say(&qs, "and W".into());
        assert!(f.complete() && !f.advance());
        assert_eq!(f.answers(&qs), ["B", "X, Y, and W"]);

        // Words replace a single choice's pick; a pick replaces words.
        f.focus(0);
        f.say(&qs, "neither".into());
        assert_eq!(f.answers(&qs)[0], "neither");
        f.pick(&qs, 0, 0);
        assert_eq!(f.answers(&qs)[0], "A");
    }

    #[test]
    fn the_arrows_stay_on_the_options() {
        let qs = [question("Which?", &["A", "B"], false)];
        let mut f = Form::new(&qs);
        f.step(&qs, -1);
        assert_eq!(f.lit, 0);
        f.step(&qs, 1);
        f.step(&qs, 1);
        assert_eq!(f.lit, 1);
    }

    #[test]
    fn numbers_from_outside_stand_for_labels() {
        let qs = [question("Which?", &["A", "B"], false), question("Why?", &["C"], false)];
        assert_eq!(resolve(&qs, &["2".into(), "because".into()]), ["B", "because"]);
        assert_eq!(resolve(&qs, &["9".into()]), ["9"]);
    }
}
