// Multi-test generator: T cases, each an array of up to 20 values.
#include <iostream>
#include <random>
#include <cstdlib>

int main(int argc, char** argv) {
    unsigned seed = (argc > 1) ? (unsigned)std::strtoul(argv[1], nullptr, 10) : 1u;
    std::mt19937 rng(seed);

    int t = 1 + (int)(rng() % 4u);
    std::cout << t << "\n";
    for (int i = 0; i < t; ++i) {
        int n = 1 + (int)(rng() % 20u);
        bool all_negative = (rng() % 3u) == 0u;
        std::cout << n << "\n";
        for (int j = 0; j < n; ++j) {
            long long v;
            if (all_negative) {
                v = -(long long)(rng() % 1000000ULL) - 1;
            } else {
                v = (long long)(rng() % 2000001ULL) - 1000000LL;
            }
            std::cout << v << (j + 1 == n ? '\n' : ' ');
        }
    }
    return 0;
}
