#![feature(custom_attribute)]
#![allow(dead_code, unused_attributes)]

#[miri_run]
fn factorial_loop() -> i64 {
    let mut product = 1;
    let mut i = 1;

    while i <= 10 {
        product *= i;
        i += 1;
    }

    product
}

#[miri_run]
fn index_for_loop() -> usize {
    let mut sum = 0;
    let a = [0, 10, 20, 30];
    for i in 0..a.len() {
        sum += a[i];
    }
    sum
}

#[miri_run]
fn for_loop() -> usize {
    let mut sum = 0;
    let a = [0, 10, 20, 30];
    for &n in &a {
        sum += n;
    }
    sum
}

#[miri_run]
fn main() {
    assert_eq!(factorial_loop(), 3628800);
    assert_eq!(index_for_loop(), 60);
    assert_eq!(for_loop(), 60);
}
