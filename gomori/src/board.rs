mod bbox;
mod bitboard;
mod compact_field;

use std::ops::Deref;

pub use bbox::*;
pub use bitboard::*;
pub use compact_field::*;

use crate::{Card, CardToPlay, CardsSet, Field, IllegalCardPlayed, Rank, Suit};

pub const BOARD_SIZE: i8 = 4;

/// Represents a board with at least one card on it.
///
/// The idea is that a list of [`Field`]s is used in the communication between judge and bots,
/// but is then converted into this type for performing the actual tasks like e.g. [determining
/// whether a card can be played](Board::possible_to_play_card).
//
// Because after the first move, there is at least one card on it,
// the minimum and maximum coordinates always exist.
#[cfg_attr(feature = "python", pyo3::pyclass)]
#[derive(Clone, Debug)]
pub struct Board {
    /// There is exactly one entry in this list for every field with at least one card on it.
    ///
    /// The `bbox` and `bitboards` fields are derived from this list.
    ///
    /// Note: This is not guaranteed to be sorted.
    fields: Vec<(i8, i8, CompactField)>,
    /// The center coordinate for all bitboards produced by this board.
    /// Using a consistent center coordinate enables binary operations like bitwise or.
    /// See also the bitboard docs.
    bitboards_center: (i8, i8),
    /// The smallest area that contains all cards.
    bbox: BoundingBox,
    /// All the diamond/heart/spade/club cards on the board.
    bitboards: [BitBoard; 4],
}

#[derive(Clone)]
struct Diff {
    flipped: BitBoard,
    won: BitBoard,
    new_card: Card,
    new_card_i: i8,
    new_card_j: i8,
}

/// The effects that playing a card would have.
///
/// Returned by [`Board::calculate()`].
pub struct CalculatedEffects<'a> {
    /// This struct ties together the board and its diff, to prevent any possible mixups
    board: &'a Board,
    diff: Diff,
    /// The cards that were won as a result of playing this card
    pub cards_won: CardsSet,
    /// Should another card be played?
    pub combo: bool,
}

// !!!!!! NOTE: Keep in sync with pymethods impl block !!!!!!
impl Board {
    /// Creates a new board from a list of [`Field`]s.
    ///
    /// Panics if the fields are (obviously) invalid, e.g. if it is larger than 4 x 4.
    pub fn new(fields: &[Field]) -> Self {
        Self::from_fields_list(
            fields
                .iter()
                .map(|f| (f.i, f.j, CompactField::from(f)))
                .collect(),
        )
    }

    /// Creates a new board from a list of [`CompactField`]s.
    ///
    /// Panics if the fields are (obviously) invalid, e.g. if it is larger than 4 x 4.
    pub fn from_fields_list(fields: Vec<(i8, i8, CompactField)>) -> Self {
        assert!(!fields.is_empty());
        let bitboards_center = (fields[0].0, fields[0].1);
        let mut bbox = BoundingBox::singleton(fields[0].0, fields[0].1);
        let mut bitboards = [BitBoard::empty_board_centered_at(bitboards_center); 4];

        for field in &fields {
            debug_assert!(field.2.top_card().is_some() || !field.2.hidden_cards().is_empty());
            bbox.update(field.0, field.1);
            if let Some(Card { suit, .. }) = field.2.top_card() {
                bitboards[suit as usize] = bitboards[suit as usize].insert(field.0, field.1);
            }
        }

        assert!(bbox.size_i() <= BOARD_SIZE as u8);
        assert!(bbox.size_j() <= BOARD_SIZE as u8);

        Self {
            fields,
            bitboards_center,
            bbox,
            bitboards,
        }
    }

    /// Calculate playing a card and return the effects that this would have.
    ///
    /// This is the core function of this type. It checks whether playing the card
    /// is legal given the other cards on the board, how many cards would be won by placing
    /// this card, and plans out the changes that would be made to the playing board.
    ///
    /// The returned struct has a method to actually apply these changes to the board, and
    /// get a new board.
    ///
    /// This function does not validate that the played card has not already been played
    /// and so on.
    pub fn calculate(
        &self,
        card_to_play: CardToPlay,
    ) -> Result<CalculatedEffects<'_>, IllegalCardPlayed> {
        let CardToPlay { i, j, card, .. } = card_to_play;

        if !self.is_in_bounds(i, j) {
            return Err(IllegalCardPlayed::OutOfBounds);
        }

        let existing_field: Option<CompactField> = self.get(i, j);

        // Check whether there is already a card on that field on which
        // the new card cannot be placed.
        if let Some(incompatible_card) = existing_field
            .and_then(|f| f.top_card())
            .filter(|&c| !card.can_be_placed_on(c))
        {
            return Err(IllegalCardPlayed::IncompatibleCard {
                existing_card: incompatible_card,
            });
        }

        // Since a field only exists when there's a card on it, existence of the
        // field means that this is a combo.
        let combo = existing_field.is_some();

        let flipped = if combo {
            // Activate the face card's abilities
            self.fields_to_flip(card_to_play)?
        } else {
            BitBoard::empty_board_centered_at(self.bitboards_center)
        };

        let won: BitBoard = {
            // A bitboard representation of all cards of the same suit as the newly
            // placed card. If there is a line of 4 cards, it must be cards of this
            // suit.
            let cards_of_same_suit = self.bitboards[card.suit as usize]
                .insert(i, j)
                .difference(flipped);
            cards_of_same_suit
                .lines_going_through_point(i, j)
                .remove(i, j)
        };

        let cards_won = {
            let mut set = CardsSet::new();
            for &(i, j, field) in &self.fields {
                if won.contains(i, j) {
                    set |= field.all_cards();
                }
            }
            set
        };

        Ok(CalculatedEffects {
            board: self,
            diff: Diff {
                flipped,
                won,
                new_card: card,
                new_card_i: i,
                new_card_j: j,
            },
            cards_won,
            combo,
        })
    }

    /// Shorthand for [`calculate()`](Board::calculate) immediately followed by [`execute()`](CalculatedEffects::execute).
    pub fn play_card(&self, card_to_play: CardToPlay) -> Result<Self, IllegalCardPlayed> {
        self.calculate(card_to_play).map(CalculatedEffects::execute)
    }

    /// The smallest area enclosing the cards currently on the board.
    ///
    /// This is always smaller than or equal to [`BOARD_SIZE`] x [`BOARD_SIZE`].
    ///
    /// See [`Self::playable_area()`] for the area where cards may be placed.
    pub fn bbox(&self) -> BoundingBox {
        self.bbox
    }

    /// The coordinates where a card may be placed.
    ///
    /// Trying to play a card outside of these bounds will result in an
    /// out-of-bounds error.
    ///
    /// Note that this area can be bigger than [`BOARD_SIZE`] x [`BOARD_SIZE`],
    /// e.g. if there's only a single card on the board so far, the area
    /// will be the 7 x 7 area centered on that card.
    pub fn playable_area(&self) -> BoundingBox {
        BoundingBox {
            i_min: self.bbox.i_max - BOARD_SIZE + 1,
            j_min: self.bbox.j_max - BOARD_SIZE + 1,
            i_max: self.bbox.i_min + BOARD_SIZE - 1,
            j_max: self.bbox.j_min + BOARD_SIZE - 1,
        }
    }

    /// The visible diamonds on the board.
    pub fn diamonds(&self) -> BitBoard {
        self.bitboards[Suit::Diamond as usize]
    }

    /// The visible hearts on the board.
    pub fn hearts(&self) -> BitBoard {
        self.bitboards[Suit::Heart as usize]
    }

    /// The visible spades on the board.
    pub fn spades(&self) -> BitBoard {
        self.bitboards[Suit::Spade as usize]
    }

    /// The visible clubs on the board.
    pub fn clubs(&self) -> BitBoard {
        self.bitboards[Suit::Club as usize]
    }

    /// Is it possible to play this card anywhere?
    ///
    /// This is a bit more efficient than checking [`Self::locations_for_card()`].
    pub fn possible_to_play_card(&self, card: Card) -> bool {
        if self.fields.len() < 16 {
            return true;
        }
        for (_, _, field) in &self.fields {
            if field.can_place_card(card) {
                return true;
            }
        }
        false
    }

    /// Returns all the coordinates that are valid places to play the given card.
    pub fn locations_for_card(&self, card: Card) -> BitBoard {
        // Create a BitBoard with 1 in every location where any card could be played
        // so that it is not out of bounds.
        let BoundingBox {
            i_min,
            j_min,
            i_max,
            j_max,
        } = self.playable_area();
        let mut bitboard = BitBoard::empty_board_centered_at(self.bitboards_center)
            .insert_area(i_min, j_min, i_max, j_max);

        for &(i, j, field) in &self.fields {
            if !field.can_place_card(card) {
                bitboard = bitboard.remove(i, j);
            }
        }
        bitboard
    }

    /// Returns all the coordinates that already have a card on them and are valid places to play the given card.
    pub fn combo_locations_for_card(&self, card: Card) -> BitBoard {
        let mut bitboard = BitBoard::empty_board_centered_at(self.bitboards_center);
        for &(i, j, field) in &self.fields {
            if field.can_place_card(card) {
                bitboard = bitboard.insert(i, j);
            }
        }
        bitboard
    }

    /// Returns a [`CompactField`] if there are any cards at the given coordinate.
    pub fn get(&self, i: i8, j: i8) -> Option<CompactField> {
        for &(i_field, j_field, compact_field) in &self.fields {
            if i_field == i && j_field == j {
                return Some(compact_field);
            }
        }
        None
    }

    pub fn is_in_bounds(&self, i: i8, j: i8) -> bool {
        (i.checked_sub(self.bbox.i_min).map(|diff| diff < BOARD_SIZE)).unwrap_or(false)
            && (self.bbox.i_max.checked_sub(i).map(|diff| diff < BOARD_SIZE)).unwrap_or(false)
            && (j.checked_sub(self.bbox.j_min).map(|diff| diff < BOARD_SIZE)).unwrap_or(false)
            && (self.bbox.j_max.checked_sub(j).map(|diff| diff < BOARD_SIZE)).unwrap_or(false)
    }

    pub fn to_fields_vec(&self) -> Vec<Field> {
        let mut fields_vec: Vec<Field> = self
            .fields
            .iter()
            .filter_map(|&(i, j, cf)| {
                if cf.is_empty() {
                    None
                } else {
                    Some(Field {
                        i,
                        j,
                        top_card: cf.top_card(),
                        hidden_cards: cf.hidden_cards().into_iter().collect(),
                    })
                }
            })
            .collect();
        fields_vec.sort_by_key(|f| (f.i, f.j));
        fields_vec
    }

    // Internal helper function to compute fields where the top cards are flipped face-down.
    //
    // Note: The result also contains empty fields and fields
    fn fields_to_flip(&self, card_to_play: CardToPlay) -> Result<BitBoard, IllegalCardPlayed> {
        let (card_i, card_j) = (card_to_play.i, card_to_play.j);
        let mut flipped = BitBoard::empty_board_centered_at(self.bitboards_center);
        match card_to_play.card.rank {
            Rank::Jack => {
                for (i, j) in [
                    (card_i - 1, card_j),
                    (card_i + 1, card_j),
                    (card_i, card_j - 1),
                    (card_i, card_j + 1),
                ] {
                    if self.is_in_bounds(i, j) {
                        flipped = flipped.insert(i, j);
                    }
                }
            }
            Rank::Queen => {
                for (i, j) in [
                    (card_i - 1, card_j - 1),
                    (card_i - 1, card_j + 1),
                    (card_i + 1, card_j - 1),
                    (card_i + 1, card_j + 1),
                ] {
                    if self.is_in_bounds(i, j) {
                        flipped = flipped.insert(i, j);
                    }
                }
            }
            Rank::King => {
                let (tgt_i, tgt_j) = card_to_play
                    .target_field_for_king_ability
                    .ok_or(IllegalCardPlayed::NoTargetForKingAbility)?;
                let field = self
                    .get(tgt_i, tgt_j)
                    .ok_or(IllegalCardPlayed::TargetForKingAbilityDoesNotExist { tgt_i, tgt_j })?;
                if field.top_card().is_none() && (card_i, card_j) != (tgt_i, tgt_j) {
                    return Err(IllegalCardPlayed::TargetForKingAbilityIsFaceDown { tgt_i, tgt_j });
                }
                flipped = flipped.insert(tgt_i, tgt_j);
            }
            _ => (), // If no face card, nothing is flipped
        }
        Ok(flipped)
    }
}

impl Deref for Board {
    type Target = [(i8, i8, CompactField)];

    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

impl<'a> CalculatedEffects<'a> {
    /// Apply the computed changes from playing the card.
    pub fn execute(self) -> Board {
        self.diff.apply(self.board)
    }
}

impl Diff {
    fn apply(self, board: &Board) -> Board {
        let mut new_fields = Vec::with_capacity(board.fields.len() + 1);
        let mut bbox = BoundingBox::singleton(self.new_card_i, self.new_card_j);
        let bitboards_center = (self.new_card_i, self.new_card_j);
        let mut bitboards = [BitBoard::empty_board_centered_at(bitboards_center); 4];
        let mut field_for_new_card_already_exists = false;

        // Copy over the fields while applying changes and updating derived
        // data (bbox and bitboards)
        for &(i, j, mut field) in board.fields.iter() {
            if self.won.contains(i, j) {
                continue;
            }
            if (i, j) == (self.new_card_i, self.new_card_j) {
                field = field.place_card(self.new_card);
                field_for_new_card_already_exists = true;
            }
            if self.flipped.contains(i, j) {
                field = field.turn_face_down()
            }
            new_fields.push((i, j, field));

            // Update derived data
            bbox.update(i, j);
            if let Some(Card { suit, .. }) = field.top_card() {
                bitboards[suit as usize] = bitboards[suit as usize].insert(i, j);
            }
        }

        // Handle the new card, if it was not placed on a preexisting field
        if !field_for_new_card_already_exists {
            let mut new_field = CompactField::new().place_card(self.new_card);
            if self.flipped.contains(self.new_card_i, self.new_card_j) {
                new_field = new_field.turn_face_down();
            } else {
                bitboards[self.new_card.suit as usize] =
                    bitboards[self.new_card.suit as usize].insert(self.new_card_i, self.new_card_j);
            }
            new_fields.push((self.new_card_i, self.new_card_j, new_field));
        }

        Board {
            bitboards_center,
            fields: new_fields,
            bbox,
            bitboards,
        }
    }
}

#[cfg(feature = "python")]
mod python {
    use pyo3::{pyclass, pymethods, Py};

    use super::*;
    use crate::{BoundingBox, CardToPlay, CompactField, IllegalCardPlayed};

    #[pyclass]
    pub struct CalculatedEffects {
        board: Py<Board>,
        diff: Diff,
        /// The cards that were won by this play,
        #[pyo3(get)]
        pub cards_won: CardsSet,
        /// Should another card be played?
        #[pyo3(get)]
        pub combo: bool,
    }

    #[pymethods]
    impl Board {
        #[new]
        fn py_from_fields_list(fields: Vec<(i8, i8, CompactField)>) -> Self {
            Self::from_fields_list(fields)
        }

        #[pyo3(name = "to_fields")]
        fn py_to_fields(&self) -> Vec<(i8, i8, CompactField)> {
            self.fields.clone()
        }

        #[pyo3(name = "calculate")]
        pub(crate) fn py_calculate(
            slf: Py<Self>,
            card_to_play: CardToPlay,
        ) -> Result<CalculatedEffects, IllegalCardPlayed> {
            let (diff, cards_won, combo) = pyo3::Python::with_gil(|py| {
                slf.borrow(py)
                    .calculate(card_to_play)
                    .map(|calc| (calc.diff, calc.cards_won, calc.combo))
            })?;
            Ok(CalculatedEffects {
                board: slf,
                diff,
                cards_won,
                combo,
            })
        }

        #[pyo3(name = "play_card")]
        fn py_play_card(&self, card_to_play: CardToPlay) -> Result<Board, IllegalCardPlayed> {
            self.play_card(card_to_play)
        }

        #[pyo3(name = "bbox")]
        fn py_bbox(&self) -> BoundingBox {
            self.bbox()
        }

        #[pyo3(name = "playable_area")]
        fn py_playable_area(&self) -> BoundingBox {
            self.playable_area()
        }

        #[pyo3(name = "diamonds")]
        fn py_diamonds(&self) -> BitBoard {
            self.diamonds()
        }

        #[pyo3(name = "hearts")]
        fn py_hearts(&self) -> BitBoard {
            self.hearts()
        }

        #[pyo3(name = "spades")]
        fn py_spades(&self) -> BitBoard {
            self.spades()
        }

        #[pyo3(name = "clubs")]
        fn py_clubs(&self) -> BitBoard {
            self.clubs()
        }

        #[pyo3(name = "possible_to_play_card")]
        fn py_possible_to_play_card(&self, card: Card) -> bool {
            self.possible_to_play_card(card)
        }

        #[pyo3(name = "locations_for_card")]
        fn py_locations_for_card(&self, card: Card) -> BitBoard {
            self.locations_for_card(card)
        }

        #[pyo3(name = "combo_locations_for_card")]
        fn py_combo_locations_for_card(&self, card: Card) -> BitBoard {
            self.combo_locations_for_card(card)
        }

        #[pyo3(name = "get")]
        fn py_get(&self, i: i8, j: i8) -> Option<CompactField> {
            self.get(i, j)
        }

        #[pyo3(name = "is_in_bounds")]
        fn py_is_in_bounds(&self, i: i8, j: i8) -> bool {
            self.is_in_bounds(i, j)
        }
    }

    #[pymethods]
    impl CalculatedEffects {
        fn execute(&self) -> Board {
            pyo3::Python::with_gil(|py| self.diff.clone().apply(&self.board.borrow(py)))
        }
    }
}

#[cfg(feature = "python")]
#[doc(hidden)]
pub use python::CalculatedEffects as PyCalculatedEffects;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use quickcheck::quickcheck;

    use super::*;
    use crate::{arbitrary::PlayCardInput, card, CardToPlay};

    quickcheck! {
        fn possible_locations_fn(input: PlayCardInput) -> bool {
            let board = Board::new(&input.fields);
            let mut more_than_zero_locations = false;
            for (i, j) in board.locations_for_card(input.card_to_play.card) {
                more_than_zero_locations = true;
                match board.calculate(CardToPlay { card: input.card_to_play.card, i, j, target_field_for_king_ability: None }) {
                    Ok(_) => {},
                    Err(IllegalCardPlayed::NoTargetForKingAbility) => {},
                    Err(_) => { return false; }
                }
            }
            more_than_zero_locations == board.possible_to_play_card(input.card_to_play.card)
        }
    }

    #[test]
    fn play_card_horizontal() {
        let board = Board::new(&[
            Field {
                i: -1,
                j: 0,
                top_card: Some(card!("4♦")),
                hidden_cards: BTreeSet::new(),
            },
            Field {
                i: -1,
                j: -1,
                top_card: Some(card!("5♦")),
                hidden_cards: BTreeSet::new(),
            },
            Field {
                i: -1,
                j: -2,
                top_card: Some(card!("6♦")),
                hidden_cards: BTreeSet::new(),
            },
            Field {
                i: -1,
                j: -3,
                top_card: Some(card!("A♠")),
                hidden_cards: BTreeSet::new(),
            },
        ]);
        let card = card!("A♦");
        let plan = board
            .calculate(CardToPlay {
                i: -1,
                j: -3,
                card,
                target_field_for_king_ability: None,
            })
            .unwrap();
        assert!(plan.diff.flipped.is_empty());
        assert_eq!(
            plan.cards_won,
            CardsSet::from_iter([card!("4♦"), card!("5♦"), card!("6♦")])
        );
    }

    #[test]
    fn play_card_antidiag() {
        let board = Board::new(&[
            Field {
                i: -1,
                j: 0,
                top_card: Some(card!("4♦")),
                hidden_cards: BTreeSet::new(),
            },
            Field {
                i: 0,
                j: -1,
                top_card: Some(card!("5♦")),
                hidden_cards: BTreeSet::new(),
            },
            Field {
                i: 1,
                j: -2,
                top_card: Some(card!("6♦")),
                hidden_cards: BTreeSet::new(),
            },
            Field {
                i: 2,
                j: -3,
                top_card: Some(card!("A♠")),
                hidden_cards: BTreeSet::new(),
            },
        ]);
        let card = card!("A♦");
        let plan = board
            .calculate(CardToPlay {
                i: 2,
                j: -3,
                card,
                target_field_for_king_ability: None,
            })
            .unwrap();
        assert!(plan.diff.flipped.is_empty());
        assert!(!plan.diff.won.is_empty());
    }
}
