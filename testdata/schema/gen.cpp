// Generates inputs the statement allows: 1 <= n <= 50, 1 <= a_i <= 1000000.
#include <iostream>
#include <random>
#include <cstdlib>

int main(int argc, char** argv) {
    unsigned seed = (argc > 1) ? (unsigned)std::strtoul(argv[1], nullptr, 10) : 1u;
    std::mt19937 rng(seed);

    int n = 1 + (int)(rng() % 50u);
    std::cout << n << "\n";
    for (int i = 0; i < n; ++i) {
        long long v = 1 + (long long)(rng() % 1000000ULL);
        std::cout << v << (i + 1 == n ? '\n' : ' ');
    }
    return 0;
}
