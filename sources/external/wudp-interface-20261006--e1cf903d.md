// WuDPQuantizer.h
#ifndef WUDP_QUANTIZER_H
#define WUDP_QUANTIZER_H

#include <vector>
#include <unordered_map>
#include <opencv2/opencv.hpp>
#include <vivid/vivid.h>
#include "ElasticColorHistogram.h" // Current histogram implementation
#include "CIELABConverter.h"

// Forward declarations
namespace Eigen { class Matrix3d; }

struct WeightedLabPoint {
    vivid::lab_t lab;
    uint32_t frequency;
    float projection_value;

    WeightedLabPoint();
    WeightedLabPoint(const vivid::lab_t& l, uint32_t f);
};

struct ProjectionBin {
    uint64_t total_frequency_in_bin;
    struct { double x, y, z; } sum_weighted_lab_in_bin_double;
    double sum_freq_L_sq;
    double sum_freq_a_sq;
    double sum_freq_b_sq;

    ProjectionBin() : total_frequency_in_bin(0), sum_weighted_lab_in_bin_double{0.0, 0.0, 0.0}, sum_freq_L_sq(0.0), sum_freq_a_sq(0.0), sum_freq_b_sq(0.0) {}
};

class WuDPQuantizer {
private:
    size_t K_target_colors_;
    ElasticColorHistogramTable<uint32_t> histogram_table_;
    CIELABConverter lab_converter_;
    std::vector<vivid::srgb_t> final_palette_srgb_;
    std::unordered_map<vivid::srgb_t, vivid::srgb_t, VividSrgbHash, VividSrgbEqualTo> color_map_cache_;
    std::unordered_map<vivid::lab_t, size_t, VividLabHash, VividLabEqualTo> lab_to_palette_index_;

    void build_palette_lookup();

public:
    WuDPQuantizer(size_t k_target_colors, uint32_t histogram_table_slots, uint32_t histogram_probe_limit = 10);
    
    void build_palette_from_image_cv(const cv::Mat& img, bool use_sampling = false, float sample_fraction = 0.1f);
    cv::Mat quantize_image_cv(const cv::Mat& img);
    const std::vector<vivid::srgb_t>& get_palette_srgb() const;
};

#endif // WUDP_QUANTIZER_H