// --- WuDPQuantizer.h (Conceptual) ---
#include "elastic_hash_table.h" // Your EHT
#include "vivid_colors.h"       // Your srgb_t, lab_t
#include "cielab_converter.h"   // Your CIEDE2000
// Other includes for WuDP (OpenCV, Eigen, etc.)

class WuDPQuantizer {
public:
    // vivid::srgb_t needs hash and equal_to for EHT if not standard types
    struct VividSrgbHash {
        std::size_t operator()(const vivid::srgb_t& srgb) const {
            // Your hash implementation
            std::size_t h1 = std::hash<float>{}(srgb.r);
            std::size_t h2 = std::hash<float>{}(srgb.g);
            std::size_t h3 = std::hash<float>{}(srgb.b);
            return h1 ^ (h2 << 1) ^ (h3 << 2); // Example
        }
    };
    struct VividSrgbEqualTo {
        bool operator()(const vivid::srgb_t& c1, const vivid::srgb_t& c2) const {
            // Your equality implementation (with tolerance for floats)
            const float epsilon = 1e-5f; // Adjust as needed
            return std::abs(c1.r - c2.r) < epsilon &&
                   std::abs(c1.g - c2.g) < epsilon &&
                   std::abs(c1.b - c2.b) < epsilon;
        }
    };

    using DeltaEHT = ElasticHashTable<vivid::srgb_t, uint32_t, VividSrgbHash, VividSrgbEqualTo>;

private:
    std::vector<vivid::srgb_t> base_palette_srgb_;
    std::vector<vivid::lab_t> base_palette_lab_;
    DeltaEHT delta_color_eht_;
    size_t k_target_colors_;
    float ciede_threshold_for_delta_;
    CIELABConverter lab_converter_; // If CIEDE2000 is a member

    std::vector<vivid::srgb_t> final_palette_srgb_;
    // ... other members for WuDP state ...

public:
    WuDPQuantizer(size_t target_colors, 
                  const std::vector<vivid::srgb_t>& initial_base_palette,
                  size_t delta_eht_capacity = 8192, 
                  float ciede_threshold = 10.0f)
        : k_target_colors_(target_colors),
          base_palette_srgb_(initial_base_palette),
          delta_color_eht_(delta_eht_capacity, 0.1), // 0.1 delta for EHT
          ciede_threshold_for_delta_(ciede_threshold) {
        
        // Convert base_palette_srgb_ to base_palette_lab_
        base_palette_lab_.reserve(base_palette_srgb_.size());
        for (const auto& srgb : base_palette_srgb_) {
            base_palette_lab_.push_back(vivid::lab::fromSrgb(srgb)); // Assuming vivid has this
        }
    }

    void build_palette_from_image(const cv::Mat& img) { // Or your image type
        delta_color_eht_ = DeltaEHT(delta_color_eht_.get_capacity(), 0.1); // Reset EHT

        // Step B: Populate DeltaColorEHT
        for (int r = 0; r < img.rows; ++r) {
            for (int c = 0; c < img.cols; ++c) {
                cv::Vec3b bgr_pixel = img.at<cv::Vec3b>(r, c); // Example with OpenCV
                vivid::srgb_t p_srgb(bgr_pixel[2]/255.f, bgr_pixel[1]/255.f, bgr_pixel[0]/255.f);
                vivid::lab_t p_lab = vivid::lab::fromSrgb(p_srgb);

                float min_dist_to_base = std::numeric_limits<float>::max();
                // Find closest in base_palette_lab_
                for (const auto& base_lab : base_palette_lab_) {
                    // CIELABConverter::Lab lab1_c = {p_lab.L, p_lab.a, p_lab.b};
                    // CIELABConverter::Lab lab2_c = {base_lab.L, base_lab.a, base_lab.b};
                    // float dist = lab_converter_.CIEDE2000(lab1_c, lab2_c);
                    // Simplified for vivid types, assuming your converter can take them or adapt
                    float dist = lab_converter_.CIEDE2000({p_lab.L,p_lab.a,p_lab.b}, {base_lab.L,base_lab.a,base_lab.b});

                    if (dist < min_dist_to_base) {
                        min_dist_to_base = dist;
                    }
                }

                if (min_dist_to_base >= ciede_threshold_for_delta_) {
                    uint32_t initial_count = 1;
                    uint32_t* count_ptr = delta_color_eht_.insert_or_get(p_srgb, initial_count);
                    if (count_ptr) {
                         // Check if it was an existing key by comparing the current value
                         // This check is a bit indirect. A boolean from insert_or_get would be cleaner.
                        bool was_newly_inserted = (*count_ptr == initial_count && 
                                                   delta_color_eht_.find(p_srgb) == count_ptr); // Heuristic to check if it's truly new
                                                                                              // A better way is for insert_or_get to return this info.
                                                                                              // Let's assume insert_or_get correctly sets to initial_count if new,
                                                                                              // and returns ptr to existing if old.
                        if (!was_newly_inserted && *count_ptr != 0 /* avoid incrementing if it was a failed new insert that got 0 */) {
                           // To make this robust, insert_or_get must reliably distinguish
                           // "found existing" from "newly inserted".
                           // Simpler:
                           // Value* found = delta_color_eht_.find(p_srgb);
                           // if(found) (*found)++; else delta_color_eht_.insert_or_get(p_srgb, 1);
                           // The current insert_or_get already does find first.
                           // So if count_ptr is not initial_count, it means it existed.
                           if (*count_ptr != initial_count) {
                               (*count_ptr)++;
                           }
                        }
                    } // else: EHT full for new delta, color lost
                }
            }
        }

        // Step C: Form Candidate Input for WuDP
        std::vector<std::pair<vivid::srgb_t, uint32_t>> wudp_input_histogram;
        
        // Add base palette colors with a pseudo-frequency
        uint32_t base_pseudo_freq = std::max(1u, (img.rows * img.cols) / ( (uint32_t)base_palette_srgb_.size() * 4 ) ); // Heuristic
        for (const auto& srgb : base_palette_srgb_) {
            wudp_input_histogram.push_back({srgb, base_pseudo_freq});
        }

        // Add delta colors
        std::vector<std::pair<vivid::srgb_t, uint32_t>> delta_entries = delta_color_eht_.get_all_entries();
        // Sort delta_entries by count if needed, or just add all. WuDP considers frequency.
        for (const auto& entry : delta_entries) {
            wudp_input_histogram.push_back(entry); // entry.first is srgb, entry.second is count
        }

        // Step D: Run WuDP
        // Your existing WuDP logic:
        // 1. `merged_lab_points = prepare_lab_data_from_histogram(wudp_input_histogram);`
        //    (This function should merge duplicate sRGBs by summing counts, then convert to LAB,
        //     then merge duplicate LABs by summing counts).
        // 2. `perform_weighted_pca_eigen(merged_lab_points, ...)`
        // 3. `project_sort_and_bin_lab_data(...)`
        // 4. `palette_lab_centroids = run_dynamic_programming_quantization(...)`
        // 5. Convert `palette_lab_centroids` to `final_palette_srgb_`.
        // ... (Placeholder for calling your WuDP functions) ...
        std::cout << "Placeholder: WuDP would run on " << wudp_input_histogram.size() 
                  << " initial candidate colors (base + delta)." << std::endl;
        // Example: if WuDP simply takes the most frequent if too many
        if (wudp_input_histogram.size() > k_target_colors_) {
             std::sort(wudp_input_histogram.begin(), wudp_input_histogram.end(), 
                [](const auto&a, const auto&b){ return a.second > b.second; });
             final_palette_srgb_.clear();
             for(size_t i=0; i < k_target_colors_ && i < wudp_input_histogram.size(); ++i) {
                 final_palette_srgb_.push_back(wudp_input_histogram[i].first);
             }
        } else {
            final_palette_srgb_.clear();
            for(const auto& entry : wudp_input_histogram) final_palette_srgb_.push_back(entry.first);
        }


        // Step E: (Done by your quantize_image_cv after this function returns)
    }

    const std::vector<vivid::srgb_t>& get_final_palette_srgb() const {
        return final_palette_srgb_;
    }
    // ... other methods ...
};