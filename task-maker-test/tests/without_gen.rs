use task_maker_format::ioi::TestcaseEvaluationStatus::*;
use task_maker_test::*;

#[test]
fn without_gen() {
    better_panic::install();

    TestInterface::new("without_gen")
        .time_limit(1.0)
        .memory_limit(64)
        .max_score(100.0)
        .subtask_scores(vec![100.0])
        .must_compile("soluzione.cpp")
        .must_compile("wa.cpp")
        .must_compile("wrong_file.cpp")
        .solution_score("soluzione.cpp", vec![100.0])
        .solution_score("wa.cpp", vec![50.0])
        .solution_score("wrong_file.cpp", vec![0.0])
        .solution_statuses("soluzione.cpp", vec![Accepted("Output is correct".into())])
        .solution_statuses(
            "wa.cpp",
            vec![
                Accepted("Output is correct".into()),
                Accepted("Output is correct".into()),
                WrongAnswer("Output is incorrect".into()),
                WrongAnswer("Output is incorrect".into()),
            ],
        )
        .solution_statuses(
            "wrong_file.cpp",
            vec![WrongAnswer("Output is incorrect".into())],
        )
        .run();
}
