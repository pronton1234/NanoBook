// An open-addressed symbol map, because std::unordered_map cannot be fast here.
//
// The standard REQUIRES std::unordered_map to be node-based: references and
// pointers to elements must stay valid across rehash, which forces a separately
// allocated node per entry. Every lookup is therefore a pointer chase into
// memory allocated at some unrelated moment, and on a workload doing one lookup
// per message that cost dominates.
//
// Rust's std HashMap is SwissTable (hashbrown) -- open-addressed, with entries
// stored inline in one contiguous array. Comparing the two languages while one
// side uses a node-based map and the other does not measures the maps, not the
// languages.
//
// So: linear probing over a flat array of slots, power-of-two capacity, with
// tombstone-free deletion avoided entirely because this map never erases -- a
// symbol seen once is seen for the rest of the session.
#pragma once

#include <cstddef>
#include <cstdint>
#include <utility>
#include <vector>

namespace ll {

template <typename Key, typename Value, typename Hash>
class FlatMap {
public:
    explicit FlatMap(std::size_t capacity = 1024) { reserve(capacity); }

    /// Find or default-construct the value for `key`.
    ///
    /// Returns a reference that is invalidated by any later insert, which is
    /// exactly the guarantee std::unordered_map gives up performance to provide.
    /// The caller here uses the reference immediately and never holds it, so the
    /// weaker contract costs nothing.
    Value& operator[](const Key& key) {
        if (size_ * 8 >= slots_.size() * 5) grow();  // keep load factor under 5/8
        std::size_t i = index_of(key);
        if (!slots_[i].occupied) {
            slots_[i].occupied = true;
            slots_[i].key = key;
            ++size_;
        }
        return slots_[i].value;
    }

    [[nodiscard]] const Value* find(const Key& key) const noexcept {
        const std::size_t i = index_of(key);
        return slots_[i].occupied ? &slots_[i].value : nullptr;
    }

    [[nodiscard]] std::size_t size() const noexcept { return size_; }
    void clear() noexcept {
        for (auto& s : slots_) s.occupied = false;
        size_ = 0;
    }

    /// Visit every live entry. Order is unspecified.
    template <typename F>
    void for_each(F&& f) const {
        for (const auto& s : slots_)
            if (s.occupied) f(s.key, s.value);
    }

private:
    struct Slot {
        Key key{};
        Value value{};
        bool occupied = false;
    };

    void reserve(std::size_t n) {
        std::size_t cap = 16;
        while (cap < n * 2) cap <<= 1;
        slots_.assign(cap, Slot{});
        mask_ = cap - 1;
        size_ = 0;
    }

    /// Slot for `key`: its own, or the first free one after it.
    [[nodiscard]] std::size_t index_of(const Key& key) const noexcept {
        std::size_t i = Hash{}(key) & mask_;
        // Linear probing. Cache-friendly on a miss chain because the next slot
        // is the next cache line, not an unrelated heap allocation.
        while (slots_[i].occupied && !(slots_[i].key == key)) i = (i + 1) & mask_;
        return i;
    }

    void grow() {
        std::vector<Slot> old = std::move(slots_);
        reserve(old.size());
        for (auto& s : old)
            if (s.occupied) (*this)[s.key] = std::move(s.value);
    }

    std::vector<Slot> slots_;
    std::size_t mask_ = 0;
    std::size_t size_ = 0;
};

}  // namespace ll
