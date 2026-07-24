//! Каркас идемпотентных шагов установки (LLD-13 п. 3.2). Профиль отдаёт
//! раннеру список шагов; каждый шаг сам решает, достигнуто ли уже целевое
//! состояние, поэтому повторный запуск установщика ничего не ломает.

use anyhow::{bail, Context, Result};

/// Итог шага в отчёте раннера.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// Целевое состояние уже было достигнуто, apply не запускался.
    AlreadyDone,
    /// apply отработал и verify подтвердил результат.
    Applied,
}

pub trait Step {
    fn name(&self) -> String;

    /// true = система уже в целевом состоянии этого шага.
    fn check(&self) -> Result<bool>;

    fn apply(&self) -> Result<()>;

    /// Пост-проверка после apply: повторный check обязан пройти.
    fn verify(&self) -> Result<()> {
        if self.check()? {
            Ok(())
        } else {
            bail!("состояние после применения не совпало с целевым")
        }
    }
}

/// Прогнать шаги по порядку. Первый упавший шаг останавливает установку:
/// дальше идти бессмысленно, а повторный запуск доведёт с этого же места.
pub fn run(steps: &[Box<dyn Step>]) -> Result<Vec<(String, StepOutcome)>> {
    let mut report = Vec::with_capacity(steps.len());
    for step in steps {
        let name = step.name();
        let done = step
            .check()
            .with_context(|| format!("шаг {name}: проверка состояния"))?;
        if done {
            println!("  [=] {name}: уже настроено");
            report.push((name, StepOutcome::AlreadyDone));
            continue;
        }
        println!("  [+] {name}");
        step.apply().with_context(|| format!("шаг {name}"))?;
        step.verify()
            .with_context(|| format!("шаг {name}: проверка после применения"))?;
        report.push((name, StepOutcome::Applied));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct FakeStep {
        done: Rc<Cell<bool>>,
        applies: Rc<Cell<u32>>,
        fail_apply: bool,
    }

    impl Step for FakeStep {
        fn name(&self) -> String {
            "fake".into()
        }
        fn check(&self) -> Result<bool> {
            Ok(self.done.get())
        }
        fn apply(&self) -> Result<()> {
            if self.fail_apply {
                bail!("сломался");
            }
            self.applies.set(self.applies.get() + 1);
            self.done.set(true);
            Ok(())
        }
    }

    fn fake(done: bool, fail_apply: bool) -> (Box<dyn Step>, Rc<Cell<bool>>, Rc<Cell<u32>>) {
        let d = Rc::new(Cell::new(done));
        let a = Rc::new(Cell::new(0));
        (
            Box::new(FakeStep {
                done: d.clone(),
                applies: a.clone(),
                fail_apply,
            }),
            d,
            a,
        )
    }

    #[test]
    fn applies_when_not_done_and_verifies() {
        let (step, done, applies) = fake(false, false);
        let report = run(&[step]).unwrap();
        assert_eq!(report[0].1, StepOutcome::Applied);
        assert!(done.get());
        assert_eq!(applies.get(), 1);
    }

    #[test]
    fn repeat_run_is_noop() {
        let (step, _, applies) = fake(false, false);
        run(std::slice::from_ref(&step)).unwrap();
        let report = run(&[step]).unwrap();
        assert_eq!(report[0].1, StepOutcome::AlreadyDone);
        assert_eq!(applies.get(), 1, "повторный запуск не должен применять шаг снова");
    }

    #[test]
    fn failed_apply_stops_the_run() {
        let (bad, _, _) = fake(false, true);
        let (tail, _, tail_applies) = fake(false, false);
        let err = run(&[bad, tail]).unwrap_err();
        assert!(err.to_string().contains("шаг fake"));
        assert_eq!(tail_applies.get(), 0, "после падения шаги не выполняются");
    }

    #[test]
    fn verify_catches_apply_that_did_not_converge() {
        struct Liar;
        impl Step for Liar {
            fn name(&self) -> String {
                "liar".into()
            }
            fn check(&self) -> Result<bool> {
                Ok(false)
            }
            fn apply(&self) -> Result<()> {
                Ok(())
            }
        }
        let err = run(&[Box::new(Liar) as Box<dyn Step>]).unwrap_err();
        assert!(err.to_string().contains("проверка после применения"));
    }
}
