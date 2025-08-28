// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod parse;

use fidl_next::ClientSender;
use fidl_next_fuchsia_examples_calculator::Calculator;
use fuchsia_component::client::fidl_next::connect_to_protocol;
use std::fs;

/// Entry-point into the client.
#[fuchsia::main]
async fn main() -> Result<(), anyhow::Error> {
    // `Calculator` is generated code. The build rule `fidl("calculator")`
    // in <../../../fidl/BUILD.gn> generates the necessary targets so
    // <../BUILD.gn> can rely on
    // `"//examples/fidl/calculator/fidl:calculator_rust_next"` to make this
    // available.
    let calculator =
        connect_to_protocol::<Calculator>().expect("Error connecting to Calculator Service.");
    let sender = calculator.sender().clone();
    let calculator_handler = fuchsia_async::Task::spawn(calculator.run_sender());

    // Note the path starts with /pkg/ even though the build rule
    // `resource("input")` uses `data/input.txt`. At runtime, components are
    // able to read the contents of their own package by accessing the path
    // /pkg/ in their namespace. See
    // https://fuchsia.dev/fuchsia-src/development/components/data#including_resources_with_a_component
    // for more details.
    let input = fs::read_to_string("/pkg/data/input.txt")?;

    for line in input.lines() {
        let result = calculator_line(line, &sender).await;
        match result {
            Ok(result) => log::info!("{} = {}", &line, result),
            Err(msg) => log::info!("Error with expression '{}': {}", &line, &msg),
        }
    }

    sender.close();
    calculator_handler.await.expect("calculator client terminated unexpectedly");

    Ok(())
}

async fn calculator_line(
    line: &str,
    calculator: &ClientSender<Calculator>,
) -> Result<f64, fidl_next::Error> {
    let parse::Expression::Leaf(left, op, right) = parse::parse(line);
    Ok(match op {
        parse::Operator::Add => *calculator.add(left, right)?.await?.sum,
        parse::Operator::Subtract => *calculator.subtract(left, right)?.await?.difference,
        parse::Operator::Multiply => *calculator.multiply(left, right)?.await?.product,
        parse::Operator::Divide => *calculator.divide(left, right)?.await?.quotient,
        parse::Operator::Pow => *calculator.pow(left, right)?.await?.power,
    })
}
