// Reference: largest element, correctly seeded.
#include <iostream>
#include <algorithm>
#include <climits>

int main() {
    int t;
    if (!(std::cin >> t)) return 0;
    while (t--) {
        int n;
        std::cin >> n;
        long long best = LLONG_MIN;
        for (int i = 0; i < n; ++i) {
            long long x;
            std::cin >> x;
            best = std::max(best, x);
        }
        std::cout << (n == 0 ? 0 : best) << "\n";
    }
    return 0;
}
