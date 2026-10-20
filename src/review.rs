//! The review gate's author set: which models wrote the code a PR carries,
//! and how a reviewer is chosen clear of them. Part B of
//! docs/design/review-gate-and-route-strength.md, as the first mate's
//! accepted revision splits it: this module holds the author set and the
//! reviewer choice; the review record, the review errand and `pr.merge`
//! (round B1) join it later.

pub mod authors;
