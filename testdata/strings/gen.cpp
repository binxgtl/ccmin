// Emit 30 words drawn from {a, A, b} on one line.
#include <iostream>
#include <random>
#include <cstdlib>

int main(int argc, char** argv) {
    unsigned seed = (argc > 1) ? (unsigned)std::strtoul(argv[1], nullptr, 10) : 1u;
    std::mt19937 rng(seed);
    const char* words[3] = {"a", "A", "b"};
    const int n = 30;
    for (int i = 0; i < n; ++i) {
        std::cout << words[rng() % 3u] << (i + 1 == n ? '\n' : ' ');
    }
    return 0;
}
