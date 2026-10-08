// WUDB.cpp
#include "WuDPQuantizer.h"
#include "ElasticColorHistogram.h"
#include "PCA.h"
#include <iostream>
#include <algorithm>
#include <cmath>
#include <numeric>
#include <chrono>
#include <Eigen/Dense>

// --- Configuration Constants ---
const int M_PROJECTION_BINS = 256;

// --- Hash Function Implementations ---
std::size_t VividSrgbHash::operator()(const vivid::srgb_t& srgb) const {
    std::size_t h1 = std::hash<float>{}(srgb.r);
    std::size_t h2 = std::hash<float>{}(srgb.g);
    std::size_t h3 = std::hash<float>{}(srgb.b);
    return h1 ^ (h2 << 1) ^ (h3 << 2);
}

bool VividSrgbEqualTo::operator()(const vivid::srgb_t& c1, const vivid::srgb_t& c2) const {
    const float epsilon = 1e-5f;
    return std::abs(c1.r - c2.r) < epsilon &&
           std::abs(c1.g - c2.g) < epsilon &&
           std::abs(c1.b - c2.b) < epsilon;
}

std::size_t VividLabHash::operator()(const vivid::lab_t& lab) const {
    int L_bin = static_cast<int>(lab.L / 5.0f);
    int a_bin = static_cast<int>(lab.a / 2.0f);
    int b_bin = static_cast<int>(lab.b / 2.0f);
    std::size_t h1 = std::hash<int>{}(L_bin);
    std::size_t h2 = std::hash<int>{}(a_bin);
    std::size_t h3 = std::hash<int>{}(b_bin);
    return h1 ^ (h2 << 1) ^ (h3 << 2);
}

bool VividLabEqualTo::operator()(const vivid::lab_t& c1, const vivid::lab_t& c2) const {
    CIELABConverter converter;
    return converter.CIEDE2000(
        {c1.L, c1.a, c1.b, 255.0f},
        {c2.L, c2.a, c2.b, 255.0f}
    ) < 3.0f;
}

// --- ElasticColorHistogramTable Implementation ---
template<typename IndexType>
ElasticColorHistogramTable<IndexType>::ElasticColorHistogramTable(IndexType n_slots, IndexType probe_lim)
    : n_(n_slots), probe_limit_(probe_lim), current_unique_colors_(0) {
    if (n_ > 0) {
        if (static_cast<unsigned long long>(n_) > std::numeric_limits<IndexType>::max()) {
            throw std::overflow_error("Table size 'n_slots' exceeds IndexType max.");
        }
        table_.resize(n_);
        arrays_config_len_ = (n_ > 1) ? static_cast<IndexType>(std::ceil(std::log2(static_cast<double>(n_)))) : 1;
        if (arrays_config_len_ == 0 && n_ > 0) arrays_config_len_ = 1;
    } else {
        arrays_config_len_ = 0;
    }
}

template<typename IndexType>
void ElasticColorHistogramTable<IndexType>::increment(const vivid::srgb_t& color) {
    if (n_ == 0) return;

    if (current_unique_colors_ > n_ * 0.7) {
        resize_table(n_ * 2);
    }

    for (IndexType i = 0; i < arrays_config_len_; ++i) {
        auto probe_seq = _probe_sequence(color, i);
        for (IndexType probe_idx : probe_seq) {
            if (!table_[probe_idx].has_value()) {
                table_[probe_idx] = ColorCount{color, 1};
                current_unique_colors_++;
                return;
            } else if (VividSrgbEqualTo{}(table_[probe_idx].value().color, color)) {
                table_[probe_idx].value().count++;
                return;
            }
        }
    }
    static bool warning_shown = false;
    if (!warning_shown && (current_unique_colors_ > n_ * 0.9)) {
        std::cerr << "Warning: ElasticColorHistogramTable is becoming very full. "
                  << "Consider increasing initial size. Some colors may be lost." << std::endl;
        warning_shown = true;
    }
}

template<typename IndexType>
std::vector<std::pair<vivid::srgb_t, uint32_t>> ElasticColorHistogramTable<IndexType>::get_histogram_data() const {
    std::vector<std::pair<vivid::srgb_t, uint32_t>> data;
    data.reserve(current_unique_colors_);
    for (const auto& slot : table_) {
        if (slot.has_value()) {
            data.push_back({slot.value().color, slot.value().count});
        }
    }
    return data;
}

template<typename IndexType>
IndexType ElasticColorHistogramTable<IndexType>::get_unique_color_count() const {
    return current_unique_colors_;
}

template<typename IndexType>
IndexType ElasticColorHistogramTable<IndexType>::get_capacity() const {
    return n_;
}

template<typename IndexType>
std::vector<IndexType> ElasticColorHistogramTable<IndexType>::_probe_sequence(const vivid::srgb_t& key_color, IndexType conceptual_array_index) {
    if (n_ == 0 || probe_limit_ == 0) return {};
    std::vector<IndexType> sequence;
    sequence.reserve(probe_limit_);
    std::mt19937 local_rng(static_cast<unsigned int>(_hash_color(key_color) ^ (conceptual_array_index << 16)));
    std::uniform_int_distribution<IndexType> dist(0, (n_ > 0) ? n_ - 1 : 0);
    for (IndexType i = 0; i < probe_limit_; ++i) sequence.push_back(dist(local_rng));
    return sequence;
}

template<typename IndexType>
std::size_t ElasticColorHistogramTable<IndexType>::_hash_color(const vivid::srgb_t& color) const {
    return VividSrgbHash{}(color);
}

template<typename IndexType>
void ElasticColorHistogramTable<IndexType>::resize_table(IndexType new_size) {
    if (new_size <= n_) return;
    ElasticColorHistogramTable new_table(new_size, probe_limit_);
    for (const auto& slot : table_) {
        if (slot.has_value()) {
            for (uint32_t i = 0; i < slot.value().count; ++i) {
                new_table.increment(slot.value().color);
            }
        }
    }
    *this = std::move(new_table);
    std::cout << "  Histogram table resized to " << n_ << " slots." << std::endl;
}

// Explicit instantiation
template class ElasticColorHistogramTable<uint32_t>;

// --- WuDPQuantizer Implementation ---
WeightedLabPoint::WeightedLabPoint() : lab({0,0,0}), frequency(0), projection_value(0.0f) {}
WeightedLabPoint::WeightedLabPoint(const vivid::lab_t& l, uint32_t f)
    : lab(l), frequency(f), projection_value(0.0f) {}

ProjectionBin::ProjectionBin() : total_frequency_in_bin(0), sum_weighted_lab_in_bin({0,0,0}),
                                sum_freq_L_sq(0.0), sum_freq_a_sq(0.0), sum_freq_b_sq(0.0) {}

WuDPQuantizer::WuDPQuantizer(size_t k_target_colors, uint32_t histogram_table_slots, uint32_t histogram_probe_limit)
    : K_target_colors_(k_target_colors), histogram_table_(histogram_table_slots, histogram_probe_limit) {
    if (K_target_colors_ == 0) throw std::invalid_argument("K_target_colors must be positive.");
    std::cout << "WuDPQuantizer initialized. Target colors: " << K_target_colors_
              << ", Hist Table Slots: " << histogram_table_slots << std::endl;
}

void WuDPQuantizer::build_palette_lookup() {
    lab_to_palette_index_.clear();
    for (size_t i = 0; i < final_palette_srgb_.size(); ++i) {
        vivid::lab_t lab = vivid::lab::fromSrgb(final_palette_srgb_[i]);
        lab_to_palette_index_[lab] = i;
    }
    std::cout << "  Palette lookup table built with " << lab_to_palette_index_.size() << " entries." << std::endl;
}

// ... (Rest of WuDPQuantizer methods: build_palette_from_image_cv, quantize_image_cv, get_palette_srgb)
// ... (Other functions: prepare_lab_data_from_histogram, perform_weighted_pca_eigen, etc.)
// ... (Main function)

std::vector<WeightedLabPoint> prepare_lab_data_from_histogram(
    const std::vector<std::pair<vivid::srgb_t, uint32_t>>& histogram_data) {
    std::cout << "  Preparing LAB data from histogram (" << histogram_data.size() << " unique colors)..." << std::endl;
    std::unordered_map<vivid::lab_t, uint32_t, VividLabHash, VividLabEqualTo> lab_histogram;
    for (const auto& entry : histogram_data) {
        vivid::lab_t lab = vivid::lab::fromSrgb(entry.first);
        lab_histogram[lab] += entry.second;
    }

    std::vector<WeightedLabPoint> lab_points;
    lab_points.reserve(lab_histogram.size());
    for (const auto& entry : lab_histogram) {
        lab_points.emplace_back(entry.first, entry.second);
    }
    std::cout << "  Reduced to " << lab_points.size() << " unique LAB colors after merging." << std::endl;
    return lab_points;
}

bool perform_weighted_pca_eigen(
    const std::vector<WeightedLabPoint>& weighted_lab_points,
    vivid::lab_t& out_principal_axis,
    vivid::lab_t& out_mean_lab) {
    std::cout << "  Performing weighted PCA..." << std::endl;
    if (weighted_lab_points.empty()) {
        std::cerr << "    PCA Error: No data points provided." << std::endl;
        out_principal_axis = {1.0f, 0.0f, 0.0f};
        out_mean_lab = {50.0f, 0.0f, 0.0f};
        return false;
    }

    uint64_t total_weight = 0;
    double sum_L = 0.0, sum_a = 0.0, sum_b = 0.0;

    for (const auto& p : weighted_lab_points) {
        const uint64_t freq = p.frequency;
        total_weight += freq;
        sum_L += static_cast<double>(p.lab.L) * freq;
        sum_a += static_cast<double>(p.lab.a) * freq;
        sum_b += static_cast<double>(p.lab.b) * freq;
    }

    if (total_weight == 0) {
        std::cerr << "    PCA Error: Total weight is zero." << std::endl;
        return false;
    }

    out_mean_lab.L = static_cast<float>(sum_L / total_weight);
    out_mean_lab.a = static_cast<float>(sum_a / total_weight);
    out_mean_lab.b = static_cast<float>(sum_b / total_weight);

    Eigen::Matrix3d cov_matrix = Eigen::Matrix3d::Zero();
    std::vector<Eigen::Matrix3d> thread_cov(1, Eigen::Matrix3d::Zero());

    for (size_t i = 0; i < weighted_lab_points.size(); ++i) {
        const auto& p = weighted_lab_points[i];
        const Eigen::Vector3d centered(
            p.lab.L - out_mean_lab.L,
            p.lab.a - out_mean_lab.a,
            p.lab.b - out_mean_lab.b
        );
        thread_cov[0] += p.frequency * (centered * centered.transpose());
    }

    cov_matrix = thread_cov[0] / total_weight;

    Eigen::SelfAdjointEigenSolver<Eigen::Matrix3d> eigen_solver(cov_matrix);
    if (eigen_solver.info() != Eigen::Success) {
        std::cerr << "    PCA Error: Eigen decomposition failed." << std::endl;
        return false;
    }

    const double max_eigenvalue = eigen_solver.eigenvalues().maxCoeff();
    if (max_eigenvalue < 1e-9) {
        std::cerr << "    PCA Warning: All eigenvalues near zero. Using default axis." << std::endl;
        out_principal_axis = {1.0f, 0.0f, 0.0f};
    } else {
        const Eigen::Vector3d principal_eigenvector = eigen_solver.eigenvectors().col(2);
        out_principal_axis = {
            static_cast<float>(principal_eigenvector[0]),
            static_cast<float>(principal_eigenvector[1]),
            static_cast<float>(principal_eigenvector[2])
        };
    }

    return true;
}

std::vector<ProjectionBin> project_sort_and_bin_lab_data(
    std::vector<WeightedLabPoint>& unique_lab_points,
    const vivid::lab_t& pca_axis,
    const vivid::lab_t& pca_mean,
    int num_bins) {
    
    std::cout << "  Projecting " << unique_lab_points.size() << " unique LAB colors onto principal axis..." << std::endl;
    if (unique_lab_points.empty()) return {};

    float min_proj = std::numeric_limits<float>::max();
    float max_proj = std::numeric_limits<float>::lowest();

    for (auto& p : unique_lab_points) {
        vivid::lab_t centered_lab = {p.lab.L - pca_mean.L, p.lab.a - pca_mean.a, p.lab.b - pca_mean.b};
        p.projection_value = centered_lab.L * pca_axis.L +
                             centered_lab.a * pca_axis.a +
                             centered_lab.b * pca_axis.b;
        if (p.projection_value < min_proj) min_proj = p.projection_value;
        if (p.projection_value > max_proj) max_proj = p.projection_value;
    }

    std::sort(unique_lab_points.begin(), unique_lab_points.end(),
              [](const WeightedLabPoint& a, const WeightedLabPoint& b) {
                  return a.projection_value < b.projection_value;
              });
    std::cout << "    Projections sorted. Range: [" << min_proj << ", " << max_proj << "]" << std::endl;

    std::vector<ProjectionBin> binned_projections(num_bins);
    if (num_bins == 0) return binned_projections;

    double bin_width = (max_proj - min_proj) / num_bins;
    if (max_proj == min_proj) bin_width = 1.0;

    for (const auto& p : unique_lab_points) {
        int bin_idx = 0;
        if (bin_width > 1e-6) {
            bin_idx = static_cast<int>((p.projection_value - min_proj) / bin_width);
        }
        bin_idx = std::max(0, std::min(num_bins - 1, bin_idx));

        ProjectionBin& current_bin = binned_projections[bin_idx];
        current_bin.total_frequency_in_bin += p.frequency;
        current_bin.sum_weighted_lab_in_bin.L += p.lab.L * p.frequency;
        current_bin.sum_weighted_lab_in_bin.a += p.lab.a * p.frequency;
        current_bin.sum_weighted_lab_in_bin.b += p.lab.b * p.frequency;
        
        current_bin.sum_freq_L_sq += static_cast<double>(p.lab.L) * p.lab.L * p.frequency;
        current_bin.sum_freq_a_sq += static_cast<double>(p.lab.a) * p.lab.a * p.frequency;
        current_bin.sum_freq_b_sq += static_cast<double>(p.lab.b) * p.lab.b * p.frequency;
    }
    std::cout << "  Data binned into " << num_bins << " projection bins." << std::endl;
    return binned_projections;
}

float calculate_dp_segment_error(
    const std::vector<ProjectionBin>& bins,
    int bin_idx_start,
    int bin_idx_end) {

    if (bin_idx_start < 0 || bin_idx_end >= bins.size() || bin_idx_start > bin_idx_end) {
        return 0.0f;
    }

    double seg_sum_L = 0, seg_sum_a = 0, seg_sum_b = 0;
    uint64_t seg_total_freq = 0;
    double seg_sum_freq_L_sq = 0, seg_sum_freq_a_sq = 0, seg_sum_freq_b_sq = 0;

    for (int i = bin_idx_start; i <= bin_idx_end; ++i) {
        seg_sum_L += bins[i].sum_weighted_lab_in_bin.L;
        seg_sum_a += bins[i].sum_weighted_lab_in_bin.a;
        seg_sum_b += bins[i].sum_weighted_lab_in_bin.b;
        seg_total_freq += bins[i].total_frequency_in_bin;
        seg_sum_freq_L_sq += bins[i].sum_freq_L_sq;
        seg_sum_freq_a_sq += bins[i].sum_freq_a_sq;
        seg_sum_freq_b_sq += bins[i].sum_freq_b_sq;
    }

    if (seg_total_freq == 0) return 0.0f;

    double error_L = seg_sum_freq_L_sq - (seg_sum_L * seg_sum_L / seg_total_freq);
    double error_a = seg_sum_freq_a_sq - (seg_sum_a * seg_sum_a / seg_total_freq);
    double error_b = seg_sum_freq_b_sq - (seg_sum_b * seg_sum_b / seg_total_freq);
    
    float total_error = static_cast<float>(error_L + error_a + error_b);
    return std::max(0.0f, total_error);
}

std::vector<vivid::lab_t> run_dynamic_programming_quantization(
    const std::vector<ProjectionBin>& projection_bins,
    int K_target_colors) {
    
    int M = projection_bins.size();
    std::cout << "  Running Dynamic Programming for K=" << K_target_colors << " on M=" << M << " bins." << std::endl;

    if (M == 0 || K_target_colors == 0) return {};
    if (K_target_colors >= M) {
        std::vector<vivid::lab_t> palette;
        for (const auto& bin : projection_bins) {
            if (bin.total_frequency_in_bin > 0) {
                palette.push_back({
                    bin.sum_weighted_lab_in_bin.L / bin.total_frequency_in_bin,
                    bin.sum_weighted_lab_in_bin.a / bin.total_frequency_in_bin,
                    bin.sum_weighted_lab_in_bin.b / bin.total_frequency_in_bin
                });
            }
        }
        while (palette.size() < (size_t)K_target_colors && !palette.empty()) palette.push_back(palette.back());
        if (palette.empty() && K_target_colors > 0) palette.assign(K_target_colors, {50,0,0});
        return palette;
    }

    std::vector<std::vector<float>> E_dp(K_target_colors + 1, std::vector<float>(M + 1, 0.0f));
    std::vector<std::vector<int>> Cut_dp(K_target_colors + 1, std::vector<int>(M + 1, 0));

    for (int m_bins = 1; m_bins <= M; ++m_bins) {
        E_dp[1][m_bins] = calculate_dp_segment_error(projection_bins, 0, m_bins - 1);
    }

    for (int k = 2; k <= K_target_colors; ++k) {
        for (int m_bins = k; m_bins <= M; ++m_bins) {
            E_dp[k][m_bins] = std::numeric_limits<float>::max();
            for (int i_cut_point = k - 1; i_cut_point < m_bins; ++i_cut_point) {
                float error_last_segment = calculate_dp_segment_error(projection_bins, i_cut_point, m_bins - 1);
                float current_total_error = E_dp[k - 1][i_cut_point] + error_last_segment;

                if (current_total_error < E_dp[k][m_bins]) {
                    E_dp[k][m_bins] = current_total_error;
                    Cut_dp[k][m_bins] = i_cut_point;
                }
            }
        }
    }

    std::vector<vivid::lab_t> palette_lab(K_target_colors);
    int current_segment_end_exclusive = M;
    for (int k = K_target_colors; k >= 1; --k) {
        int segment_start_inclusive = (k == 1) ? 0 : Cut_dp[k][current_segment_end_exclusive];
        
        double sum_L = 0, sum_a = 0, sum_b = 0;
        uint64_t total_freq_in_segment = 0;
        for (int bin_idx = segment_start_inclusive; bin_idx < current_segment_end_exclusive; ++bin_idx) {
            sum_L += projection_bins[bin_idx].sum_weighted_lab_in_bin.L;
            sum_a += projection_bins[bin_idx].sum_weighted_lab_in_bin.a;
            sum_b += projection_bins[bin_idx].sum_weighted_lab_in_bin.b;
            total_freq_in_segment += projection_bins[bin_idx].total_frequency_in_bin;
        }

        if (total_freq_in_segment > 0) {
            palette_lab[k - 1] = {
                static_cast<float>(sum_L / total_freq_in_segment),
                static_cast<float>(sum_a / total_freq_in_segment),
                static_cast<float>(sum_b / total_freq_in_segment)
            };
        } else {
            palette_lab[k-1] = (k < K_target_colors && !palette_lab.empty()) ? palette_lab[k] : vivid::lab_t{50.f,0.f,0.f};
        }
        current_segment_end_exclusive = segment_start_inclusive;
    }
    std::cout << "  Dynamic Programming finished." << std::endl;
    return palette_lab;
}

void WuDPQuantizer::build_palette_from_image_cv(
    const cv::Mat& img,
    bool use_sampling,
    float sample_fraction) {
    
    std::cout << "Starting palette generation..." << std::endl;
    auto overall_start_time = std::chrono::high_resolution_clock::now();

    std::cout << "Step 1: Building histogram..." << std::endl;
    histogram_table_ = ElasticColorHistogramTable<uint32_t>(histogram_table_.get_capacity(), 10);
    color_map_cache_.clear();
    final_palette_srgb_.clear();
    lab_to_palette_index_.clear();

    auto start_hist_time = std::chrono::high_resolution_clock::now();
    if (use_sampling) {
        int total_pixels = img.rows * img.cols;
        int num_samples = static_cast<int>(total_pixels * sample_fraction);
        if (num_samples <= 0 && total_pixels > 0) num_samples = total_pixels;
        else if (total_pixels == 0) { std::cerr << "Empty image for sampling." << std::endl; return; }
        
        std::random_device rd;
        std::mt19937 rng(rd());
        std::uniform_int_distribution<int> dist_row(0, img.rows - 1);
        std::uniform_int_distribution<int> dist_col(0, img.cols - 1);

        for (int i = 0; i < num_samples; ++i) {
            int r = dist_row(rng);
            int c = dist_col(rng);
            cv::Vec3b bgr_pixel = img.at<cv::Vec3b>(r, c);
            vivid::srgb_t srgb_color(bgr_pixel[2]/255.f, bgr_pixel[1]/255.f, bgr_pixel[0]/255.f);
            histogram_table_.increment(srgb_color);
        }
    } else {
        for (int r = 0; r < img.rows; ++r) {
            for (int c = 0; c < img.cols; ++c) {
                cv::Vec3b bgr_pixel = img.at<cv::Vec3b>(r, c);
                vivid::srgb_t srgb_color(bgr_pixel[2]/255.f, bgr_pixel[1]/255.f, bgr_pixel[0]/255.f);
                histogram_table_.increment(srgb_color);
            }
        }
    }
    auto end_hist_time = std::chrono::high_resolution_clock::now();
    std::cout << "  Histogram built in "
              << std::chrono::duration_cast<std::chrono::milliseconds>(end_hist_time - start_hist_time).count() << "ms. "
              << "Unique colors: " << histogram_table_.get_unique_color_count() << std::endl;

    std::vector<std::pair<vivid::srgb_t, uint32_t>> histogram_data = histogram_table_.get_histogram_data();
    if (histogram_data.empty()) {
        std::cerr << "Warning: Histogram is empty. Cannot generate palette." << std::endl;
        return;
    }

    if (histogram_table_.get_unique_color_count() <= K_target_colors_) {
        std::cout << "  Low-color image detected (" << histogram_table_.get_unique_color_count()
                  << " <= " << K_target_colors_ << "). Using histogram colors directly." << std::endl;
        final_palette_srgb_.clear();
        for (const auto& entry : histogram_data) {
            final_palette_srgb_.push_back(entry.first);
        }
        while (final_palette_srgb_.size() < K_target_colors_ && !final_palette_srgb_.empty()) {
            final_palette_srgb_.push_back(final_palette_srgb_.back());
        }
        if (final_palette_srgb_.empty()) {
            final_palette_srgb_.assign(K_target_colors_, {0.5f, 0.5f, 0.5f});
        }
        build_palette_lookup();
        return;
    }

    std::cout << "Step 2: Preparing LAB data..." << std::endl;
    std::vector<WeightedLabPoint> unique_weighted_lab_points = prepare_lab_data_from_histogram(histogram_data);

    std::cout << "Step 3: Performing PCA..." << std::endl;
    vivid::lab_t pca_axis, pca_mean;
    auto start_pca_time = std::chrono::high_resolution_clock::now();
    if (!perform_weighted_pca_eigen(unique_weighted_lab_points, pca_axis, pca_mean)) {
        std::cerr << "Error: PCA failed. Cannot generate palette." << std::endl;
        return;
    }
    auto end_pca_time = std::chrono::high_resolution_clock::now();
    std::cout << "  PCA completed in "
              << std::chrono::duration_cast<std::chrono::milliseconds>(end_pca_time - start_pca_time).count() << "ms." << std::endl;
    std::cout << "    Principal Axis: L=" << pca_axis.L << ", a=" << pca_axis.a << ", b=" << pca_axis.b << std::endl;
    std::cout << "    Mean LAB: L=" << pca_mean.L << ", a=" << pca_mean.a << ", b=" << pca_mean.b << std::endl;

    std::cout << "Step 4: Projecting, sorting, and binning data..." << std::endl;
    auto start_bin_time = std::chrono::high_resolution_clock::now();
    std::vector<ProjectionBin> binned_projections =
        project_sort_and_bin_lab_data(unique_weighted_lab_points, pca_axis, pca_mean, M_PROJECTION_BINS);
    auto end_bin_time = std::chrono::high_resolution_clock::now();
    std::cout << "  Projection and Binning completed in "
              << std::chrono::duration_cast<std::chrono::milliseconds>(end_bin_time - start_bin_time).count() << "ms." << std::endl;

    std::cout << "Step 5: Running Dynamic Programming..." << std::endl;
    auto start_dp_time = std::chrono::high_resolution_clock::now();
    std::vector<vivid::lab_t> palette_lab_centroids =
        run_dynamic_programming_quantization(binned_projections, static_cast<int>(K_target_colors_));
    auto end_dp_time = std::chrono::high_resolution_clock::now();
    std::cout << "  Dynamic Programming completed in "
              << std::chrono::duration_cast<std::chrono::milliseconds>(end_dp_time - start_dp_time).count() << "ms." << std::endl;
    
    if (palette_lab_centroids.empty()) {
        std::cerr << "Warning: Dynamic Programming returned no centroids." << std::endl;
        return;
    }

    final_palette_srgb_.clear();
    for (const auto& lab_centroid : palette_lab_centroids) {
        final_palette_srgb_.push_back(vivid::srgb::fromLab(lab_centroid));
    }
    
    build_palette_lookup();

    auto overall_end_time = std::chrono::high_resolution_clock::now();
    std::cout << "Palette generation finished. Total time: "
              << std::chrono::duration_cast<std::chrono::milliseconds>(overall_end_time - overall_start_time).count() << "ms." << std::endl;
    std::cout << "  Final palette size: " << final_palette_srgb_.size() << std::endl;
}

cv::Mat WuDPQuantizer::quantize_image_cv(const cv::Mat& img) {
    if (img.empty()) throw std::runtime_error("Input image for quantization is empty.");
    if (img.type() != CV_8UC3) throw std::runtime_error("Input image must be CV_8UC3.");

    if (final_palette_srgb_.empty()) {
        std::cerr << "Error: Palette is empty. Call build_palette_from_image_cv first. Returning original image." << std::endl;
        return img.clone();
    }
    
    std::cout << "Quantizing image (" << img.rows << "x" << img.cols << ") with "
              << final_palette_srgb_.size() << " palette colors..." << std::endl;
    cv::Mat quantized_img = cv::Mat::zeros(img.rows, img.cols, CV_8UC3);
    auto start_quant = std::chrono::high_resolution_clock::now();

    for (int r = 0; r < img.rows; ++r) {
        for (int c = 0; c < img.cols; ++c) {
            cv::Vec3b bgr_pixel = img.at<cv::Vec3b>(r, c);
            vivid::srgb_t original_srgb(bgr_pixel[2]/255.f, bgr_pixel[1]/255.f, bgr_pixel[0]/255.f);
            vivid::srgb_t mapped_srgb;

            vivid::lab_t original_lab = vivid::lab::fromSrgb(original_srgb);
            auto it_lab = lab_to_palette_index_.find(original_lab);
            if (it_lab != lab_to_palette_index_.end()) {
                mapped_srgb = final_palette_srgb_[it_lab->second];
            } else {
                auto it_srgb = color_map_cache_.find(original_srgb);
                if (it_srgb != color_map_cache_.end()) {
                    mapped_srgb = it_srgb->second;
                } else {
                    CIELABConverter::Lab original_cielab_lab = {original_lab.L, original_lab.a, original_lab.b, 255.0f};
                    float min_dist = std::numeric_limits<float>::max();
                    size_t best_idx = 0;

                    for (size_t i = 0; i < final_palette_srgb_.size(); ++i) {
                        vivid::lab_t palette_lab = vivid::lab::fromSrgb(final_palette_srgb_[i]);
                        float dist = lab_converter_.CIEDE2000(
                            original_cielab_lab,
                            {palette_lab.L, palette_lab.a, palette_lab.b, 255.0f}
                        );
                        if (dist < min_dist) {
                            min_dist = dist;
                            best_idx = i;
                        }
                    }
                    mapped_srgb = final_palette_srgb_[best_idx];
                    color_map_cache_[original_srgb] = mapped_srgb;
                    lab_to_palette_index_[original_lab] = best_idx;
                }
            }

            quantized_img.at<cv::Vec3b>(r, c) = cv::Vec3b(
                static_cast<uchar>(std::max(0.f, std::min(255.f, std::round(mapped_srgb.b * 255.f)))),
                static_cast<uchar>(std::max(0.f, std::min(255.f, std::round(mapped_srgb.g * 255.f)))),
                static_cast<uchar>(std::max(0.f, std::min(255.f, std::round(mapped_srgb.r * 255.f))))
            );
        }
    }
    auto end_quant = std::chrono::high_resolution_clock::now();
    std::cout << "Pixel assignment (quantization) complete in "
              << std::chrono::duration_cast<std::chrono::milliseconds>(end_quant - start_quant).count() << "ms." << std::endl;
    return quantized_img;
}

const std::vector<vivid::srgb_t>& WuDPQuantizer::get_palette_srgb() const {
    return final_palette_srgb_;
}

int main(int argc, char* argv[]) {
    if (argc < 3) {
        std::cerr << "Usage: " << argv[0] << " <input_image_path> <output_image_path> [K_colors] [hist_table_slots] [use_sampling (0/1)] [sample_fraction (0.0-1.0)]" << std::endl;
        std::cerr << "Defaults: K=16, hist_slots=262144, sampling=0, sample_frac=0.1. M_PROJECTION_BINS is fixed at " << M_PROJECTION_BINS << std::endl;
        return 1;
    }

    std::string input_path = argv[1];
    std::string output_path = argv[2];
    size_t k_colors = (argc > 3) ? std::stoul(argv[3]) : 16;
    uint32_t hist_slots = (argc > 4) ? std::stoul(argv[4]) : 262144;
    bool use_sampling = (argc > 5) ? (std::stoi(argv[5]) != 0) : false;
    float sample_frac = (argc > 6) ? std::stof(argv[6]) : 0.1f;

    cv::Mat image = cv::imread(input_path, cv::IMREAD_COLOR);
    if (image.empty()) {
        std::cerr << "Failed to load image: " << input_path << std::endl;
        return 1;
    }
    std::cout << "Loaded image: " << input_path << " (" << image.cols << "x" << image.rows << ")" << std::endl;

    try {
        WuDPQuantizer quantizer(k_colors, hist_slots);
        
        auto total_start_time = std::chrono::high_resolution_clock::now();
        quantizer.build_palette_from_image_cv(image, use_sampling, sample_frac);
        
        const auto& palette = quantizer.get_palette_srgb();
        std::cout << "\nFinal Palette (sRGB values scaled 0-255):" << std::endl;
        if (palette.empty()) {
            std::cout << "  Palette is empty." << std::endl;
        }
        for (size_t i = 0; i < palette.size(); ++i) {
            const auto& c = palette[i];
            std::cout << "  Color " << i << ": R:" << static_cast<int>(std::round(c.r * 255.f))
                      << " G:" << static_cast<int>(std::round(c.g * 255.f))
                      << " B:" << static_cast<int>(std::round(c.b * 255.f)) << std::endl;
        }

        cv::Mat quantized_image = quantizer.quantize_image_cv(image);
        auto total_end_time = std::chrono::high_resolution_clock::now();
        std::cout << "Total processing time (palette + quantization): "
                  << std::chrono::duration_cast<std::chrono::milliseconds>(total_end_time - total_start_time).count() << "ms." << std::endl;

        if (!cv::imwrite(output_path, quantized_image)) {
            std::cerr << "Failed to save quantized image to: " << output_path << std::endl;
            return 1;
        }
        std::cout << "Quantized image saved to " << output_path << std::endl;

    } catch (const std::exception& e) {
        std::cerr << "Error: " << e.what() << std::endl;
        return 1;
    }
    return 0;
}