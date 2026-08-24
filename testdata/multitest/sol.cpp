// For each test case print the largest element.
#include <iostream>
#include <algorithm>

int main() {
    int t;
    if (!(std::cin >> t)) return 0;
    while (t--) {
        int n;
        std::cin >> n;
        long long best = 0;   // BUG: wrong for all-negative arrays
        for (int i = 0; i < n; ++i) {
            long long x;
            std::cin >> x;
            best = std::max(best, x);
        }
        std::cout << best << "\n";
    }
    return 0;
}
