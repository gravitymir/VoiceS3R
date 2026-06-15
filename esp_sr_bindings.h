// Extra bindgen header for the ESP-SR (speech recognition) component.
// esp-idf-sys generates Rust bindings for these into `esp_idf_svc::sys::esp_sr`.
#include "esp_afe_config.h"   // afe_config_init, afe_config_t, afe_type_t, afe_mode_t
#include "esp_afe_sr_models.h" // esp_afe_handle_from_config -> esp_afe_sr_iface_t*
#include "esp_afe_sr_iface.h" // esp_afe_sr_iface_t (feed/fetch/get_feed_chunksize/get_feed_channel_num/create_from_config), afe_fetch_result_t
#include "esp_wn_iface.h"     // wakenet_state_t: WAKENET_DETECTED / WAKENET_NO_DETECT
#include "model_path.h"       // srmodel_list_t, esp_srmodel_init, esp_srmodel_filter, ESP_WN_PREFIX
