#include <algorithm>
#include <iostream>
#include <vector>

int main() {
    int n;
    if (!(std::cin >> n)) return 0;
    std::vector<int> degree(n + 1);
    for (int i = 0; i + 1 < n; ++i) {
        int u, v;
        std::cin >> u >> v;
        ++degree[u];
        ++degree[v];
    }
    std::cout << *std::max_element(degree.begin(), degree.end()) << '\n';
}
