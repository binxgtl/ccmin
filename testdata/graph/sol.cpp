#include <iostream>

int main() {
    int n, m;
    if (!(std::cin >> n >> m)) return 0;
    int incident_to_one = 0;
    for (int i = 0; i < m; ++i) {
        int u, v;
        std::cin >> u >> v;
        incident_to_one += (u == 1 || v == 1);
    }
    // BUG: reports only edges incident to vertex 1.
    std::cout << incident_to_one << '\n';
}
