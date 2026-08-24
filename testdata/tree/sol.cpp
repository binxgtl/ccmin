#include <iostream>

int main() {
    int n;
    if (!(std::cin >> n)) return 0;
    int degree_of_one = 0;
    for (int i = 0; i + 1 < n; ++i) {
        int u, v;
        std::cin >> u >> v;
        degree_of_one += (u == 1 || v == 1);
    }
    // BUG: reports the degree of vertex 1, not the maximum degree.
    std::cout << degree_of_one << '\n';
}
