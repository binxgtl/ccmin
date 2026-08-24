#include <iostream>

int main() {
    const int n = 8;
    std::cout << n << '\n';
    for (int vertex = 1; vertex <= n; ++vertex) {
        if (vertex != 2) std::cout << 2 << ' ' << vertex << '\n';
    }
}
