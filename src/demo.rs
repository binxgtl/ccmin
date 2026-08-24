//! A self-contained worked example, so a first-time user sees the tool work
//! before writing any C++. `ccmin --demo` materialises these three files in a
//! temporary directory and runs the normal pipeline over them.
//!
//! The bug is the classic Kadane mistake: seeding `best` at 0 silently allows
//! the empty subarray, so any all-negative input gives 0 instead of the
//! largest element. It shrinks from 100 values down to a single `-1`.

pub const SOL: &str = r#"// Maximum subarray sum -- O(n) Kadane.
#include <iostream>
#include <vector>
#include <algorithm>

int main() {
    int n;
    if (!(std::cin >> n)) return 0;
    std::vector<long long> a(n);
    for (int i = 0; i < n; ++i) std::cin >> a[i];

    long long best = 0, cur = 0;   // BUG: allows the empty subarray
    for (int i = 0; i < n; ++i) {
        cur = std::max(0LL, cur + a[i]);
        best = std::max(best, cur);
    }
    std::cout << best << "\n";
    return 0;
}
"#;

pub const BRUTE: &str = r#"// Maximum subarray sum -- O(n^2) reference, non-empty subarray required.
#include <iostream>
#include <vector>
#include <algorithm>
#include <climits>

int main() {
    int n;
    if (!(std::cin >> n)) return 0;
    std::vector<long long> a(n);
    for (int i = 0; i < n; ++i) std::cin >> a[i];

    if (n == 0) { std::cout << 0 << "\n"; return 0; }

    long long best = LLONG_MIN;
    for (int i = 0; i < n; ++i) {
        long long sum = 0;
        for (int j = i; j < n; ++j) {
            sum += a[j];
            best = std::max(best, sum);
        }
    }
    std::cout << best << "\n";
    return 0;
}
"#;

pub const GEN: &str = r#"// Random array generator. argv[1] is the seed.
#include <iostream>
#include <random>
#include <cstdlib>

int main(int argc, char** argv) {
    unsigned seed = (argc > 1) ? (unsigned)std::strtoul(argv[1], nullptr, 10) : 1u;
    std::mt19937 rng(seed);

    const int n = 100;
    // One run in three is all-negative, which is where the bug lives.
    bool all_negative = (rng() % 3u) == 0u;

    std::cout << n << "\n";
    for (int i = 0; i < n; ++i) {
        long long v;
        if (all_negative) {
            v = -(long long)(rng() % 1000000000ULL) - 1;
        } else {
            v = (long long)(rng() % 2000000001ULL) - 1000000000LL;
        }
        std::cout << v << (i + 1 == n ? '\n' : ' ');
    }
    return 0;
}
"#;
