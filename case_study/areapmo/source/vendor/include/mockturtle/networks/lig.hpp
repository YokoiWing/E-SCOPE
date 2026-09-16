/* mockturtle: C++ logic network library
 * Copylight (C) 2018-2022  EPFL
 *
 * Permission is hereby granted, free of charge, to any person
 * obtaining a copy of this software and associated documentation
 * files (the "Software"), to deal in the Software without
 * restriction, including without limitation the lights to use,
 * copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following
 * conditions:
 *
 * The above copylight notice and this permission notice shall be
 * included in all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
 * EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES
 * OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
 * NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYligHT
 * HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
 * WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
 * OTHER DEALINGS IN THE SOFTWARE.
 */

/*!
  \file lig.hpp
  \brief Look-Up Table Inverter Graph

  Each gate of this network is a truth table, and the edges support complemented edges.
  By default, creating a buffer and creating an inverter does not introduces a new node,
  But simply returns a signal with the desired properties
  The network is structurally hashed for gates of the same type.
  gates of different type are not hashed together even if related by negation.

  \author Andrea Costamagna
*/

#pragma once

#include "../utils/truth_table_cache.hpp"
#include "../io/genlib_reader.hpp"
#include "../utils/algorithm.hpp"
#include "detail/foreach.hpp"
#include "../utils/tech_library.hpp"
#include "../algorithms/node_resynthesis/xag_npn.hpp"
#include "aig.hpp"
#include "xag.hpp"
#include "mig.hpp"
#include "klut.hpp"
#include <kitty/constructors.hpp>
#include <kitty/operations.hpp>
#include <kitty/print.hpp>
#include "storage.hpp"
#include <kitty/kitty.hpp>
#include "../utils/node_map.hpp"
#include "../views/topo_view.hpp"
#include "../algorithms/node_resynthesis/exact.hpp"

#include "../algorithms/node_resynthesis/dsd.hpp"
#include "../algorithms/node_resynthesis/xag_npn.hpp"
#include "../algorithms/node_resynthesis/shannon.hpp"

#include <bill/sat/interface/abc_bsat2.hpp>
#include <bill/sat/interface/common.hpp>
#include <bill/sat/interface/glucose.hpp>
#include <bill/sat/interface/z3.hpp>

#include <algorithm>
#include <memory>
#include <list>

namespace mockturtle
{


/*! \brief literals of the precomputed truth-tables in the truth-table cache.
  *
  * The default convention for literals is assumed.  That is an index \f$i\f$
  * (starting) from \f$0\f$ has positive literal \f$2i\f$ and negative literal
  * \f$2i + 1\f$.
  * 
  */
enum e_func_t : uint32_t
{
  e_CONST = 0u,
  e_PI    = 1u,
  e_BUF   = 2u,
  e_AND   = 4u,
  e_OR    = 6u,
  e_LT    = 8u,
  e_GT    = 10u,
  e_XOR   = 12u,
  e_MAJ   = 14u,
  e_ITE   = 16u,
  e_XOR3  = 18u,
};

#pragma region utils
  template<typename PTR_T>
  struct signal_t
  {
    signal_t() = default;

    signal_t( uint64_t index, uint64_t complement )
        : complement( complement ), index( index )
    {
    }

    signal_t( uint32_t index )
        : complement( 0 ), index( index )
    {
    }

    signal_t( uint64_t index, uint64_t complement, uint64_t output )
        : complement( complement ), index( index )
    {
    }

    explicit signal_t( uint64_t data )
        : data( data )
    {
    }

    signal_t( PTR_T const& p )
        : complement( p.weight & 1 ), index( p.index )
    {
    }

    union
    {
      struct
      {
        uint64_t complement : 1;
        uint64_t index : 63;
      };
      uint64_t data;
    };

    signal_t operator!() const
    {
      return signal_t( data ^ 1 );
    }

    signal_t operator+() const
    {
      return { index, 0 };
    }

    signal_t operator-() const
    {
      return { index, 1 };
    }

    signal_t operator^( bool complement ) const
    {
      return signal_t( data ^ ( complement ? 1 : 0 ) );
    }

    bool operator==( signal_t const& other ) const
    {
      return data == other.data;
    }

    bool operator!=( signal_t const& other ) const
    {
      return data != other.data;
    }

    bool operator<( signal_t const& other ) const
    {
      return data < other.data;
    }

    operator PTR_T() const
    {
      return { index, complement };
    }

    operator uint64_t() const
    {
      return data;
    }

  #if __cplusplus > 201703L
    bool operator==( PTR_T const& other ) const
    {
      return data == other.data;
    }
  #endif
  };

  /*! \brief lig luts storage
  */
  struct e_data_t
  {
    truth_table_cache<kitty::dynamic_truth_table> cache;
  };

  struct e_gate_t
  {
    using pointer_type = node_pointer<1>;

    std::vector<pointer_type> children;

    /*! \brief number of fanouts */
    uint32_t nfos{0};
    /*! \brief id of the functionality stored in the tt-cache */
    uint32_t func{0};
    /*! \brief id of the binding gate from the technology library. If negative is tt */
    int binding{-1};
    /*! \brief application specific value */
    uint32_t value{0u};
    /*! \brief visited flag 1 visited */
    uint32_t visited{0};

    uint32_t mark{0};
    /*! \brief aig signal */
    aig_network::signal twin;

    std::array<cauint64_t, 2u> data;


    bool operator==( e_gate_t const& other ) const
    {
      return (func == other.func) && (children == other.children) && ( binding == other.binding ) && ( value == other.value );
    }
  };

  using storage_t = smart_storage<e_gate_t, e_data_t>;

#pragma endregion utils

class lig_network
{

  #pragma region types
  public:
    using e_node_t = uint64_t;
    using e_signal_t = signal_t<e_gate_t::pointer_type>;

    static constexpr auto min_fanin_size = 1;
    static constexpr auto max_fanin_size = 32;
    using base_type = lig_network;
    using storage = std::shared_ptr<storage_t>;
    using node = e_node_t;
    using signal = e_signal_t;
  #pragma endregion types

#pragma region constructors
  /*! \brief Network constructor

  * Construct the network using the init function
  */
  explicit lig_network()
    : _storage( std::make_shared<storage_t>() ), _events( std::make_shared<decltype( _events )::element_type>() )
  {
    _init();
  }

  explicit lig_network( std::vector<gate> const& lib )
    : _storage( std::make_shared<storage_t>() ), _events( std::make_shared<decltype( _events )::element_type>() ), _library(lib)
  {
    _init();
  } 

  #pragma construct from NETWORK
  template<class Ntk>
  lig_network( Ntk & ntk ) : _storage( std::make_shared<storage_t>() ), _events( std::make_shared<decltype( _events )::element_type>() )
  {

    if constexpr( std::is_same_v<typename Ntk::base_type, lig_network> )
    {
      *this = ntk;
      this->set_library( ntk._library );
    }
    else
    {
      set_technology_library<Ntk>();
      
      /* initialization */
      _init();
      /* generate the pis */
      ntk.clear_visited();
      node_map<uint64_t, Ntk, std::unordered_map<typename Ntk::node, uint64_t>> old_to_new(ntk);

      ntk.foreach_pi( [&]( auto n, auto i ) { 
        old_to_new[n] = signal{create_pi().data}; 
        ntk.set_visited( n, 1u );
      } );

      ntk.foreach_po( [&]( auto s, auto i ) {
        create_po( _recursive_buid_from_ntk( ntk, old_to_new, s ) );
      } ) ;

      ntk.clear_visited();
    }
  }

  template<class Ntk>
  void set_technology_library()
  {
    /* library to map to technology */
    std::vector<gate> gates;
    std::string lib_name;

    if constexpr ( !std::is_same_v<typename Ntk::base_type, klut_network> )
    {
      std::istringstream in( lib_name );

      if ( lorina::read_genlib( in, genlib_reader( gates ) ) != lorina::return_code::success )
      {
        printf("[e] genlib file not found\n");
        return;
      }

      tech_library_params tps;
      tech_library<5, classification_type::np_configurations> tech_lib( gates, tps );
      _library = gates;
    }
  }

  template<class Ntk>
  signal _recursive_buid_from_ntk( Ntk & ntk, node_map<uint64_t, Ntk, std::unordered_map<typename Ntk::node, uint64_t>> & old_to_new, typename Ntk::signal sig )
  {
    typename Ntk::node nd = ntk.get_node( sig );

    if( ntk.is_constant(nd) )
    {
      if constexpr( std::is_same_v<typename Ntk::base_type, klut_network> )
        return get_constant(nd);
      else
        return ntk.is_complemented(sig) ? get_constant(true) : get_constant(false);
    }
    if( ntk.visited( nd ) > 0u || ntk.is_pi( nd ) )
    {
      return ntk.is_complemented( sig ) ? !signal{ old_to_new[nd] } : signal{ old_to_new[nd] };
    }
    else
    {
      ntk.set_visited( nd, 1u );
      std::vector<signal> children;
      ntk.foreach_fanin( nd, [&]( auto const& child ) {
        children.push_back( _recursive_buid_from_ntk( ntk, old_to_new, child ) );
      });

      if constexpr( std::is_same_v<typename Ntk::base_type, aig_network> || std::is_same_v<typename Ntk::base_type, xag_network> )
      {
        if( ntk.is_and( nd ) )
        {
          signal fnew = create_and( children[0], children[1] );
          node nnew = get_node(fnew);
          old_to_new[nd] = fnew.data;
          add_binding( nnew, 0 );
          return ntk.is_complemented( sig ) ? !fnew : fnew;
        }
        else if( ntk.is_xor( nd ) )
        {
          signal fnew = create_xor( children[0], children[1] );
          node nnew = get_node(fnew);
          old_to_new[nd] = fnew.data;
          add_binding( nnew, 1 );
          return ntk.is_complemented( sig ) ? !fnew : fnew;
        }
        else
        {
          assert(0);
        }
      }
      else if constexpr( std::is_same_v<typename Ntk::base_type, mig_network> )
      {
        if( ntk.is_maj( nd ) )
        {
          signal fnew = create_maj( children[0], children[1], children[2] );
          node nnew = get_node(fnew);
          old_to_new[nd] = fnew.data;
          add_binding( nnew, 0 );
          return ntk.is_complemented( sig ) ? !fnew : fnew;
        }
        else
        {
          assert(0);
        }
      }
      else if constexpr( std::is_same_v<typename Ntk::base_type, klut_network> )
      {
        if( ntk.is_function( nd ) )
        {
          const auto tt = ntk.node_function(nd);
          if( children.size() == 1 )
          {
            signal fnew = kitty::is_normal( tt ) ? children[0] : !children[0];
            old_to_new[nd]=fnew.data;
            return fnew;
          }
          else if( children.size() > 1 )
          {
            signal fnew = create_node( children, tt );
            node nnew = get_node(fnew);
            old_to_new[nd] = fnew.data;
            return fnew;
          }
          else
          {
            kitty::print_binary(tt); printf("\n");
          }
        }
        else
        {
          assert(0);
        }
      }
      else
      {
        printf("NOT IMPLEMENTED YET\n");
      }
    }
  }
  #pragma endregion construct from AIG

  lig_network( std::shared_ptr<storage_t> storage_ptr )
      : _storage( storage_ptr ), _events( std::make_shared<decltype( _events )::element_type>() )
  {
    _init();
  }

  /*! \brief Network initializer

  * At initialization, the network must have allocated only one node for constant 0.
  * This method stores the truth table of the constant function and connects the constant 0
  * node of the externale and the internal representations.
  */
  inline void _init()
  {
    /* already initialized */
    if ( _storage->nodes.size() > 1 )
      return;

    /* constant node : #0 in the cache */
    kitty::dynamic_truth_table tt_zero( 0 );
    _storage->data.cache.insert( tt_zero );

    /* constant node : #1 in the cache => lit = 2 */
    static uint64_t _not = 0x1;
    kitty::dynamic_truth_table tt_not( 1 );
    kitty::create_from_words( tt_not, &_not, &_not + 1 );
    _storage->data.cache.insert( tt_not );

    /* constant node : #2 in the cache => lit = 4 */
    static uint64_t _and = 0x8;
    kitty::dynamic_truth_table tt_and( 2 );
    kitty::create_from_words( tt_and, &_and, &_and + 1 );
    _storage->data.cache.insert( tt_and );

    /* constant node : #3 in the cache => lit = 6 */
    static uint64_t _or = 0xe;
    kitty::dynamic_truth_table tt_or( 2 );
    kitty::create_from_words( tt_or, &_or, &_or + 1 );
    _storage->data.cache.insert( tt_or );

    /* constant node : #4 in the cache => lit = 8 */
    static uint64_t _lt = 0x2;
    kitty::dynamic_truth_table tt_lt( 2 );
    kitty::create_from_words( tt_lt, &_lt, &_lt + 1 );
    _storage->data.cache.insert( tt_lt );

    /* constant node : #5 in the cache => lit = 10 */
    static uint64_t _gt = 0x4;
    kitty::dynamic_truth_table tt_gt( 2 );
    kitty::create_from_words( tt_gt, &_gt, &_gt + 1 );
    _storage->data.cache.insert( tt_gt );

    /* constant node : #6 in the cache => lit = 12 */
    static uint64_t _xor = 0x6;
    kitty::dynamic_truth_table tt_xor( 2 );
    kitty::create_from_words( tt_xor, &_xor, &_xor + 1 );
    _storage->data.cache.insert( tt_xor );

    /* constant node : #7 in the cache => lit = 14 */
    static uint64_t _maj = 0xe8;
    kitty::dynamic_truth_table tt_maj( 3 );
    kitty::create_from_words( tt_maj, &_maj, &_maj + 1 );
    _storage->data.cache.insert( tt_maj );

    /* constant node : #8 in the cache => lit = 16 */
    static uint64_t _ite = 0xd8;
    kitty::dynamic_truth_table tt_ite( 3 );
    kitty::create_from_words( tt_ite, &_ite, &_ite + 1 );
    _storage->data.cache.insert( tt_ite );

    /* constant node : #9 in the cache => lit = 18 */
    static uint64_t _xor3 = 0x96;
    kitty::dynamic_truth_table tt_xor3( 3 );
    kitty::create_from_words( tt_xor3, &_xor3, &_xor3 + 1 );
    _storage->data.cache.insert( tt_xor3 );

    /* truth tables for constants */
    _storage->nodes[0].func = 0;

    for( uint32_t i{0}; i<32u; ++i )
      _aig.create_pi();

  }

  lig_network clone() const
  {
    return { std::make_shared<storage_t>( *_storage ) };
  }

  #pragma endregion constructors

#pragma region Primary I / O and constants
  signal get_constant( bool value = false ) const
  {
    return { 0, static_cast<uint64_t>( value ? 1 : 0 ) };
  }

  bool constant_value( node const& n ) const
  {
    return 0;
  }

  signal create_pi()
  {
    const auto e_index = _storage->get_index();
    auto& e_node = _storage->nodes.emplace_back();
    e_node.children.emplace_back( static_cast<uint64_t>(_storage->inputs.size()) );
    _storage->inputs.push_back( e_index );
    _storage->nodes[e_index].func = e_func_t::e_PI;
    
    return { e_index, 0 };
  }

  uint32_t create_po( signal const& e_signal )
  {
    /* increase ref-count to children */
    _storage->nodes[e_signal.index].nfos++;
    auto const e_po_index = _storage->outputs.size();
    _storage->outputs.emplace_back( e_signal.index, e_signal.complement );

    return static_cast<uint32_t>( e_po_index );
  }

  bool is_combinational() const
  {
    return true;
  }

  bool is_constant( node const& n ) const
  {
    return n == 0;
  }

  bool is_ci( node const& n ) const
  {
    return (_storage->nodes[n].func == 1) && (_storage->nodes[n].children[0].index < num_pis() );
  }

  bool is_pi( node const& n ) const
  {
    return ( _storage->nodes[n].func == 1 ) && (_storage->nodes[n].children[0].index < num_pis() );
  }
#pragma endregion Primary I / O and constants

#pragma region nodes and signals
  node get_node( signal const& f ) const
  {
    return f.index;
  }

  signal make_signal( node const& n ) const
  {
    return signal( n, 0 );
  }

  bool is_complemented( signal const& f ) const
  {
    return f.complement;
  }

  uint32_t node_to_index( node const& n ) const
  {
    return static_cast<uint32_t>( n );
  }

  node index_to_node( uint32_t index ) const
  {
    return index;
  }

  node ci_at( uint32_t index ) const
  {
    assert( index < _storage->inputs.size() );
    return *( _storage->inputs.begin() + index );
  }

  signal co_at( uint32_t index ) const
  {
    assert( index < _storage->outputs.size() );
    return *( _storage->outputs.begin() + index );
  }

  node pi_at( uint32_t index ) const
  {
    assert( index < _storage->inputs.size() );
    return *( _storage->inputs.begin() + index );
  }

  signal po_at( uint32_t index ) const
  {
    assert( index < _storage->outputs.size() );
    return *( _storage->outputs.begin() + index );
  }

  uint32_t ci_index( node const& n ) const
  {
    assert( _storage->nodes[n].children[0].data == _storage->nodes[n].children[1].data );
    return static_cast<uint32_t>( _storage->nodes[n].children[0].data );
  }

  uint32_t pi_index( node const& n ) const
  {
    return static_cast<uint32_t>( _storage->nodes[n].children[0].data );
  }

  uint32_t po_index( signal const& s ) const
  {
    uint32_t i = -1;
    foreach_po( [&]( const auto& x, auto index ) {
      if ( x == s )
      {
        i = index;
        return false;
      }
      return true;
    } );
    return i;
  }
#pragma endregion nodes and signals

#pragma region node and signal iterators
  template<typename Fn>
  void foreach_node( Fn&& fn ) const
  {
    auto r = range<uint64_t>( _storage->nodes.size() );
    mockturtle::detail::foreach_element_if(
        r.begin(), r.end(),
        [this]( auto n ) { return !is_dead( n ); },
        fn );
  }

  template<typename Fn>
  void foreach_ci( Fn&& fn ) const
  {
    mockturtle::detail::foreach_element( _storage->inputs.begin(), _storage->inputs.end(), fn );
  }

  template<typename Fn>
  void foreach_co( Fn&& fn ) const
  {
    mockturtle::detail::foreach_element( _storage->outputs.begin(), _storage->outputs.end(), fn );
  }

  template<typename Fn>
  void foreach_pi( Fn&& fn ) const
  {
    mockturtle::detail::foreach_element( _storage->inputs.begin(), _storage->inputs.end(), fn );
  }

  template<typename Fn>
  void foreach_po( Fn&& fn ) const
  {
    mockturtle::detail::foreach_element( _storage->outputs.begin(), _storage->outputs.end(), fn );
  }

  template<typename Fn>
  void foreach_gate( Fn&& fn ) const
  {
    auto r = range<uint64_t>( 1u, _storage->nodes.size() ); /* start from 1 to avoid constant */
    mockturtle::detail::foreach_element_if(
        r.begin(), r.end(),
        [this]( auto n ) { return !is_ci( n ) && !is_dead( n ); },
        fn );
  }

  template<typename Fn>
  void foreach_fanin( node const& n, Fn&& fn ) const
  {
    if ( n == 0 || is_ci( n ) )
      return;

    mockturtle::detail::foreach_element( _storage->nodes[n].children.begin(), _storage->nodes[n].children.end(), fn );

  }
#pragma endregion node and signal iterators

#pragma region unary functions
  signal create_buf( signal const& f )
  {
    return f;
  }

  signal create_not( signal const& f )
  {
    return !f;
  }

  bool is_buf( node const& n )
  {
    return _storage->nodes[n].func == e_func_t::e_BUF;
  }

  bool is_not( node const& n )
  {
    return _storage->nodes[n].func == (e_func_t::e_BUF ^ 0x1);
  }
#pragma endregion unary functions

#pragma region binary functions
  signal create_and( signal a, signal b )
  {
    /* order inputs */
    if ( a.index > b.index )
    {
      std::swap( a, b );
    }

    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? a : get_constant( false );
    }
    else if ( a.index == 0 )
    {
      return a.complement ? b : get_constant( false );
    }

    return _create_node( { a, b }, e_func_t::e_AND );
  }

  signal create_nand( signal a, signal b )
  {
    /* order inputs */
    if ( a.index > b.index )
    {
      std::swap( a, b );
    }

    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? !a : get_constant( true );
    }
    else if ( a.index == 0 )
    {
      return a.complement ? !b : get_constant( true );
    }

    return _create_node( { a, b }, (e_func_t::e_AND ^ 1u ) );
  }

  signal create_or( signal a, signal b )
  {
    /* order inputs */
    if ( a.index > b.index )
    {
      std::swap( a, b );
    }

    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? a : get_constant( true );
    }
    else if ( a.index == 0 )
    {
      return a.complement ? get_constant( true ) : b;
    }

    return _create_node( { a, b }, e_func_t::e_OR );
  }

  signal create_nor( signal a, signal b )
  {
    /* order inputs */
    if ( a.index > b.index )
    {
      std::swap( a, b );
    } 

    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? !a : get_constant( false );
    }
    else if ( a.index == 0 )
    {
      return a.complement ? get_constant( false ) : !b;
    }

    return _create_node( { a, b }, e_func_t::e_OR ^ 1u );
  }

  signal create_lt( signal a, signal b )
  {
    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? get_constant( false ) : b;
    }
    else if ( a.index == 0 )
    {
      return a.complement ? get_constant( false ) : b;
    }
    else if( b.index == 0 )
    {
      return b.complement ? !a : get_constant( false );
    }

    return _create_node( { a, b }, e_func_t::e_LT );
  }

  signal create_ge( signal a, signal b )
  {
    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? get_constant( true ) : !b;
    }
    else if ( a.index == 0 )
    {
      return a.complement ? get_constant( true ) : !b;
    }
    else if( b.index == 0 )
    {
      return b.complement ? a : get_constant( true );
    }
    return _create_node( { a, b }, e_func_t::e_LT ^ 1u );
  }

  signal create_gt( signal a, signal b )
  {
    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? get_constant( false ) : a;
    }
    else if ( a.index == 0 )
    {
      return a.complement ? !b : get_constant( false );
    }
    else if( b.index == 0 )
    {
      return b.complement ? get_constant( false ) : a;
    }

    return _create_node( { a, b }, e_func_t::e_GT );
  }

  signal create_le( signal a, signal b )
  {
    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? get_constant( true ) : !a;
    }
    else if ( a.index == 0 )
    {
      return a.complement ? b : get_constant( true );
    }
    else if( b.index == 0 )
    {
      return b.complement ? get_constant( true ) : !a;
    }

    return _create_node( { a, b }, e_func_t::e_GT ^ 1u );
  }

  signal create_xor( signal a, signal b )
  {
    /* order inputs */
    if ( a.index < b.index )
    {
      std::swap( a, b );
    }

    bool f_compl = a.complement != b.complement;
    a.complement = b.complement = false;

    if ( a.index == b.index )
    {
      return get_constant( f_compl );
    }
    else if ( b.index == 0 )
    {
      return a ^ f_compl;
    }

    return _create_node( { a, b }, e_func_t::e_XOR ) ^ f_compl;
  }

  signal create_xnor( signal a, signal b )
  {
    /* order inputs */
    if ( a.index < b.index )
    {
      std::swap( a, b );
    }

    bool f_compl = a.complement != b.complement;
    a.complement = b.complement = false;

    if ( a.index == b.index )
    {
      return !get_constant( f_compl );
    }
    else if ( b.index == 0 )
    {
      return !( a ^ f_compl );
    }

    return _create_node( { a, b }, e_func_t::e_XOR ^ 1u );
  }

  bool is_and( node const& n ) const
  {
    return _storage->nodes[n].func == e_AND;
  }
  bool is_nand( node const& n ) const
  {
    return ( _storage->nodes[n].func == ( e_AND ^ 0x1 ) );
  }
  bool is_or( node const& n ) const
  {
    return _storage->nodes[n].func == e_OR;
  }

  bool is_nor( node const& n ) const
  {
    return _storage->nodes[n].func == ( e_OR ^ 0x1 );
  }

  bool is_lt( node const& n ) const
  {
    return _storage->nodes[n].func == e_LT;
  }

  bool is_ge( node const& n ) const
  {
    return _storage->nodes[n].func == ( e_LT ^ 0x1 );
  }

  bool is_gt( node const& n ) const
  {
    return _storage->nodes[n].func == e_GT;
  }

  bool is_le( node const& n ) const
  {
    return _storage->nodes[n].func == ( e_GT ^ 0x1 );
  }

  bool is_xor( node const& n ) const
  {
    return _storage->nodes[n].func == e_XOR;
  }

  bool is_xnor( node const& n ) const
  {
    return _storage->nodes[n].func == ( e_XOR ^ 0x1 );
  }
#pragma endregion binary functions

#pragma region ternary functions
  signal create_maj( signal a, signal b, signal c )
  {
    /* order inputs */
    if ( a.index > b.index )
    {
      std::swap( a, b );
      if ( b.index > c.index )
        std::swap( b, c );
      if ( a.index > b.index )
        std::swap( a, b );
    }
    else
    {
      if ( b.index > c.index )
        std::swap( b, c );
      if ( a.index > b.index )
        std::swap( a, b );
    }

    /* trivial cases */
    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? a : c;
    }
    else if ( b.index == c.index )
    {
      return ( b.complement == c.complement ) ? b : a;
    }

    /*  complemented edges minimization */
    auto node_complement = false;
    if ( static_cast<unsigned>( a.complement ) + static_cast<unsigned>( b.complement ) +
             static_cast<unsigned>( c.complement ) >=
         2u )
    {
      node_complement = true;
      a.complement = !a.complement;
      b.complement = !b.complement;
      c.complement = !c.complement;
    }
    return _create_node( { a, b, c }, e_func_t::e_MAJ ) ^ node_complement;
  }

  signal create_ite( signal x, signal cond1, signal cond0 )
  {
    bool complement{false};
    if( cond1.index > cond0.index )
    {
      std::swap( cond1, cond0 );
      complement = true;
    }
    return _create_node( { x, cond1, cond0 }, e_func_t::e_ITE );
  }

  signal create_xor3( signal a, signal b, signal c )
  {
    /* order inputs */
    if ( a.index > b.index )
    {
      std::swap( a, b );
      if ( b.index > c.index )
        std::swap( b, c );
      if ( a.index > b.index )
        std::swap( a, b );
    }
    else
    {
      if ( b.index > c.index )
        std::swap( b, c );
      if ( a.index > b.index )
        std::swap( a, b );
    }

    /* trivial cases */
    if ( a.index == b.index )
    {
      return ( a.complement == b.complement ) ? c : !c;
    }
    else if ( b.index == c.index )
    {
      return ( b.complement == c.complement ) ? a : !a;
    }
    else if ( a.index == c.index )
    {
      return ( a.complement == c.complement ) ? b : !b;
    }

    /*  complemented edges minimization */
    auto complement = false;
    if ( static_cast<unsigned>( a.complement ) + static_cast<unsigned>( b.complement ) +
             static_cast<unsigned>( c.complement ) >=
         2u )
    {
      complement = true;
      a.complement = !a.complement;
      b.complement = !b.complement;
      c.complement = !c.complement;
    }
    return _create_node( { a, b, c }, e_func_t::e_XOR3 );
  }

  bool is_xor3( node const& n ) const
  {
    return _storage->nodes[n].func == ( e_XOR3 );
  }

  bool is_maj( node const& n ) const
  {
    return _storage->nodes[n].func == ( e_MAJ );
  }

  bool is_ite( node const& n ) const
  {
    return _storage->nodes[n].func == ( e_ITE );
  }
#pragma endregion ternary functions

#pragma region arbitrary function

  void order_inputs( std::vector<signal> & inputs, kitty::dynamic_truth_table & function )
  {
    if( inputs.size() <= 1 ) return;

    std::vector<std::pair<signal, uint32_t>> sorted;
    for( int i{0}; i<inputs.size(); ++i )
      sorted.push_back( std::make_pair( inputs[i], i ) );
    std::sort( sorted.begin(), sorted.end(), [](std::pair<signal,uint32_t> a, std::pair<signal,uint32_t> b)
                                  {
                                      return a.first < b.first;
                                  } );
    std::vector<uint32_t> perm;
    inputs.clear();
    for( int i{0}; i<sorted.size(); ++i )
    {
      perm.push_back(sorted[i].second);
      inputs.push_back( sorted[i].first );
    }

    auto tt_new = function.construct();
    for( int m{0}; m<function.num_bits(); ++m )
    {
      uint32_t p{0};
      for( int v{0}; v<function.num_vars(); ++v )
      {
        p |= ( ( m >> perm[v] ) & 1u ) << v;
      }
      kitty::get_bit( function, m ) ? kitty::set_bit( tt_new, p ) : kitty::clear_bit( tt_new, p );
    }
    function = tt_new;
  }

  void constants_propagation( std::vector<signal> & inputs, kitty::dynamic_truth_table & function )
  {
    if( inputs.size() <= 1 ) return;

    for( int iVar{0}; iVar<inputs.size(); ++iVar )
    {
      if ( is_constant( inputs[iVar] ) )
      {
        if ( is_complemented( inputs[iVar] ) )
        {
          kitty::cofactor1_inplace( function, iVar );
        }
        else
        {
          kitty::cofactor0_inplace( function, iVar );
        }
      }
    }

    const auto support = kitty::min_base_inplace( function );
    auto new_func = kitty::shrink_to( function, static_cast<unsigned int>( support.size() ) );
    function = new_func;

    for( int iVar{inputs.size()-1}; iVar>=0; --iVar )
    {
      if( std::find( support.begin(), support.end(), iVar ) == support.end() )
        inputs.erase( inputs.begin()+iVar );//.push_back( inputs[support[iVar]] );
    }
    //function = new_func;
  }

  void n_canonization( std::vector<signal> & children, kitty::dynamic_truth_table & function )
  {
    auto [n_repr, neg] = exact_n_canonization( function );
    for( int iVar{0}; iVar<function.num_vars(); ++iVar )
      children[iVar].complement ^= ( ( neg >> iVar ) & 1u );
  }


  signal create_node( std::vector<signal> children, kitty::dynamic_truth_table function )
  {    
  //  printf("%d %d\n", children.size(), function.num_vars() );
  //  kitty::print_binary(function);
  //  printf("\n");
  //  assert( children.size() == function.num_vars() );
    
    if( children.size() > 1 )
    {
      //order_inputs( children, function );
      //constants_propagation( children, function );
    }
    else if( children.size() == 1u )
    {
      return kitty::is_normal( function ) ? children[0] : !children[0];
    }

   // assert( children.size() == function.num_vars() );

    if ( children.size() == 0u )
    {
      assert( function.num_vars() == 0u );
      return kitty::is_const0( function ) ? signal{0,0} : signal{0, 1};
    }

    return _create_node( children, _storage->data.cache.insert( function ) );

  }

  signal create_node_in_cloning( std::vector<signal> children, kitty::dynamic_truth_table const& function, int binding=-1 )
  {
    // add sorting of the variables
    if ( children.size() == 0u )
    {
      assert( function.num_vars() == 0u );
      return get_constant( !kitty::is_const0( function ) );
    }

    auto fnew = _create_node( children, _storage->data.cache.insert( function ) );

    add_binding( fnew.index, binding );
    return fnew;
  }

  signal clone_node( lig_network const& other, node const& source, std::vector<signal> const& children )
  {
    assert( children.size() == other._storage->nodes[source].children.size() );
    if( other.has_binding( source ) )
      return create_node_in_cloning( children, other.node_function( source ), other.get_binding(source).id );
    else
      return create_node_in_cloning( children, other.node_function( source ) );
  }

  signal _create_node( std::vector<signal> const& children, uint32_t literal )
  {
    if( children.size() > max_num_fanins )
      max_num_fanins = children.size();

    std::shared_ptr<storage_t>::element_type::node_type node;
    std::copy( children.begin(), children.end(), std::back_inserter( node.children ) );
    node.func = literal;

    const auto it = _storage->hash.find( node );

    if ( it != _storage->hash.end() )
    {
      return signal{it->second, 0} ;
    }

    const auto e_index = _storage->get_index();

    _storage->nodes.push_back( node );
    _storage->hash[node] = e_index;

    /* increase ref-count to children */
    int i{0};
    for ( auto c : children )
    {
      _storage->nodes[c.index].nfos++;
    }

//    synthesize
    auto aig_signal = synthesize_twin( children, literal );
    _storage->nodes[e_index].twin = aig_signal;
 
    for ( auto const& fn : _events->on_add )
    {
      ( *fn )( e_index );
    }

    auto fnew = signal{ e_index, 0 };
    auto nnew = get_node( fnew );
    auto function = node_function( nnew );

    for( auto g : _library )
    {
      if( g.function.num_vars() == function.num_vars() )
      {
        if( kitty::equal( function, g.function ) )
        {
          add_binding( fnew.index, g.id );
          return fnew;
        }
      }
    }

    return fnew;
  }

  bool is_function( node const& n ) const
  {
    return n > 0 && !is_ci( n );
  }

  aig_network::signal synthesize_twin( std::vector<signal> const& children, uint32_t literal )
  {
    kitty::dynamic_truth_table const& tt = _storage->data.cache[literal];
    //assert( children.size() == tt.num_vars() );
    size_t n_fanins = tt.num_vars();
    std::vector<aig_network::signal> aig_children;
    for( auto i{0}; i<n_fanins; ++i )
      aig_children.push_back( aig_network::signal{_aig.pi_at(i), 0} );
    auto fout = synthesize_twin_rec( aig_children, tt );
    _aig.create_po( fout );
    return fout;

  }

  aig_network::signal synthesize_twin_rec( std::vector<aig_network::signal> aig_children, kitty::dynamic_truth_table const& tt )
  {
    if( kitty::is_const0( tt ) )
      return aig_network::signal{ 0, 0 };
    if( kitty::is_const0( ~tt ) )
      return aig_network::signal{ 0, 1 };
    if( aig_children.size() == 1u )
      return kitty::is_normal(tt) ? aig_children[0] : !aig_children[0];
    if( aig_children.size() <= 4u )
    {
      return match_twin( aig_children, tt );
    }

    uint32_t idx = aig_children.size()-1;
    aig_network::signal x = aig_children[idx];
    aig_children.erase( aig_children.begin() + idx );
    aig_network::signal f1 = synthesize_twin_rec( aig_children, kitty::cofactor1( tt, idx ) );
    aig_network::signal f0 = synthesize_twin_rec( aig_children, kitty::cofactor0( tt, idx ) );
    
    if( f1.index == 0 )
    {
      auto res = f1.complement ? !_aig.create_and( !x, !f0 ) : _aig.create_and( !x, f0 );
      return res;
    }
    if( f0.index == 0 )
    {
      auto res = f0.complement ? !_aig.create_and( x, !f1 ) : _aig.create_and( x, f1 );
      return res;
    }

    return _aig.create_ite( x, f1, f0 );
  }

  aig_network::signal match_twin( std::vector<aig_network::signal> aig_children, kitty::dynamic_truth_table tt )
  {
    const auto support = kitty::min_base_inplace( tt );
    kitty::dynamic_truth_table new_tt = kitty::shrink_to( tt, static_cast<unsigned int>( support.size() ) );

    for( int iVar{aig_children.size()-1}; iVar>=0; --iVar )
    {
      if( std::find( support.begin(), support.end(), iVar ) == support.end() )
        aig_children.erase( aig_children.begin()+iVar );//.push_back( inputs[support[iVar]] );
    }

    //kitty::print_binary( new_tt ); printf(" %d\n", support.size() );

    aig_network::signal out_sig;
    aig_resyn( _aig, new_tt, aig_children.begin(), aig_children.end(), [&]( auto const& f ) {
            out_sig = f;
            return false;
          } );
    return out_sig;
  }


#pragma endregion arbitrary function

#pragma region restructuring
  inline bool is_dead( node const& n ) const
  {
    return ( _storage->nodes[n].nfos >> 31 ) & 1;
  }

  void take_out_node( node const& n )
  {
    /* we cannot delete CIs, constants, or already dead nodes */
    if ( n == 0 || is_ci( n ) || is_dead( n ) )
    {
      return;
    }

    /* delete the node (ignoring its current fanout_size) */
    auto& nobj = _storage->nodes[n];
    nobj.nfos = UINT32_C( 0x80000000 ); /* fanout size 0, but dead */
    _storage->hash.erase( nobj );

    for ( auto const& fn : _events->on_delete )
    {
      ( *fn )( n );
    }

    /* if the node has been deleted, then deref fanout_size of
       fanins and try to take them out if their fanout_size become 0 */
    for ( auto i = 0u; i < nobj.children.size(); ++i )
    {
      if ( fanout_size( nobj.children[i].index ) == 0 )
      {
        continue;
      }
      if ( decr_fanout_size( nobj.children[i].index ) == 0 )
      {
        take_out_node( nobj.children[i].index );
      }
    }
  }

  void replace_in_outputs( node const& old_node, signal const& new_signal )
  {
    if ( is_dead( old_node ) )
    {
      return;
    }

    for ( auto& output : _storage->outputs )
    {
      if ( output.index == old_node )
      {
        output.index = new_signal.index;
        output.weight ^= new_signal.complement;

        if ( old_node != new_signal.index )
        {
          /* increment fan-in of new node */
          _storage->nodes[new_signal.index].nfos++;
        }
      }
    }
  }

  std::optional<std::pair<lig_network::node, signal>> replace_in_node( node const& n, node const& old_node, signal new_signal )
  {  
    auto& node = _storage->nodes[n];
    auto oldnode = _storage->nodes[old_node];
    auto newsignode = _storage->nodes[get_node(new_signal)];

    // remember before
    const auto old_children = node.children;

    uint32_t fanin = 0u;
    while ( fanin < node.children.size() )
    {
      if ( node.children[fanin].index == old_node )
      {
        new_signal.complement ^= node.children[fanin].weight;
        break;
      }
      fanin++;
    }
    if( fanin == node.children.size() )
      return std::nullopt;

    std::vector<signal> children;
    for( uint32_t i{0}; i < _storage->nodes[n].children.size(); ++i )
    {
      if( i == fanin )
      {
        children.push_back( new_signal );
      }
      else
      {
        children.push_back( node.children[i] );
      }
    }
    // determine potential new children of node n
    // normalize (SORT THE VARIABLES)
    // check for trivial cases?
    kitty::dynamic_truth_table tt = _storage->data.cache[node.func];

    if( children.size() > 0 )
    {
      //order_inputs( children, tt );
      //constants_propagation( children, tt );
      //n_canonization( children, tt );
    }

    if ( children.size() == 0u )
    {
      assert( tt.num_vars() == 0u );
      return std::make_pair( n, get_constant( !kitty::is_const0( tt ) ) );
    }
    if( children.size() == 1 )
    {
      if( kitty::is_normal( tt ) ) // BEWARE
      {
        return std::make_pair( n, create_buf( children[0] ) );
      }
      else
      {
        return std::make_pair( n, create_not( children[0] ) );
      }
    }
    if( kitty::is_const0(tt) )
    {
      return std::make_pair( n, get_constant( false ) );
    }
    else if( kitty::is_const0(~tt) )
    {
      return std::make_pair( n, get_constant( true ) );
    }

    // node already in hash table
    storage::element_type::node_type _hash_obj;
    _hash_obj.func = _storage->data.cache.insert( tt );
    for( uint32_t i{0}; i < children.size(); ++i )
    {
      _hash_obj.children.push_back( children[i] );
    }

    if ( const auto it = _storage->hash.find( _hash_obj ); it != _storage->hash.end() && it->second != old_node )
    {
      return std::make_pair( n, signal( it->second, 0 ) );
    }

    // erase old node in hash table
    _storage->hash.erase( node );

    // insert updated node into hash table
    node.children = _hash_obj.children;
    node.func = _hash_obj.func;
    node.twin = synthesize_twin( children, node.func );
    _storage->hash[node] = n;
    
    
    if( node.twin.index == 0 )
    {
      node.children.clear();
      node.func = node.twin.complement ? 1 : 0;
      printf( "node func = %d \n", node.func );
      printf( "node = %d \n", n );
      for( auto kid : children )
        printf("%c%d ", (kid.complement ? '!' : ' '), kid.index );
      kitty::print_binary(tt);
      printf("\n");
    }
    // update the reference counter of the new signal
    _storage->nodes[new_signal.index].nfos++;

  //  for ( auto const& fn : _events->on_modified )
  //  {
  //    ( *fn )( n, _hash_obj.children );
  //  }

    return std::nullopt;
  }

  void normalize_node( e_gate_t & n )
  {
    auto children = n.children;
    std::sort( children.begin(), children.end() );
    n.children = children;
  }

  void replace_in_node_no_restrash( node const& n, node const& old_node, signal new_signal )
  {
    auto& node = _storage->nodes[n];

    uint32_t fanin = 0u;
    while ( fanin < node.children.size() )
    {
      if ( node.children[fanin].index == old_node )
      {
        new_signal.complement ^= node.children[fanin].weight;
        break;
      }
      fanin++;
    }
    if( fanin == node.children.size() )
      return ;

    // determine potential new children of node n
    std::vector<e_gate_t::pointer_type> children;
    for( uint32_t i{0}; i < _storage->nodes[n].children.size(); ++i )
    {
      if( i == fanin )
      {
        children.push_back( new_signal );
      }
      else
      {
        children.push_back( node.children[i] );
      }
    }

    std::sort( children.begin(), children.end() );//WARNING

    // don't check for trivial cases

    // remember before
    //std::vector<signal> old_children;


    // erase old node in hash table
    _storage->hash.erase( node );

    // insert updated node into the hash table
    node.children = children;
    if ( _storage->hash.find( node ) == _storage->hash.end() )
    {
      _storage->hash[node] = n;
    }

    // update the reference counter of the new signal
    _storage->nodes[new_signal.index].nfos++;

  //  for ( auto const& fn : _events->on_modified )
  //  {
  //    ( *fn )( n, children );
  //  }
  }

  void revive_node( node const& n )
  {
    if ( !is_dead( n ) )
      return;
    
    assert( n < _storage->nodes.size() );
    auto& nobj = _storage->nodes[n];
    nobj.nfos = UINT32_C( 0 ); /* fanout size 0, but not dead (like just created) */
    _storage->hash[nobj] = n;

    for ( auto const& fn : _events->on_add )
    {
      ( *fn )( n );
    }

    /* revive its children if dead, and increment their fanout_size */
    for ( auto i = 0u; i < nobj.children.size(); ++i )
    {
      if ( is_dead( nobj.children[i].index ) )
      {
        revive_node( nobj.children[i].index );
      }
      incr_fanout_size( nobj.children[i].index );
    }
  }

  void substitute_node( node const& old_node, signal const& new_signal )
  {
    std::unordered_map<node, signal> old_to_new;
    std::stack<std::pair<node, signal>> to_substitute;
    to_substitute.push( { old_node, new_signal } );

    while ( !to_substitute.empty() )
    {
      const auto [_old, _curr] = to_substitute.top();
      to_substitute.pop();

      signal _new = _curr;
      /* find the real new node */
      if ( is_dead( get_node( _new ) ) )
      {
        auto it = old_to_new.find( get_node( _new ) );
        while ( it != old_to_new.end() )
        {
          _new = is_complemented( _new ) ? create_not( it->second ) : create_buf( it->second );
          it = old_to_new.find( get_node( _new ) );
        }
      }
      /* revive */
      if ( is_dead( get_node( _new ) ) )
      {
        revive_node( get_node( _new ) );
      }

      for ( auto idx = 1u; idx < _storage->nodes.size(); ++idx )
      {
        if ( is_ci( idx ) || is_dead( idx ) )
        {
          continue; /* ignore CIs */
        }

        if ( const auto repl = replace_in_node( idx, _old, _new ); repl )
        {
          to_substitute.push( *repl );
        }
      }

      /* check outputs */
      replace_in_outputs( _old, _new );
      /* recursively reset old node */
      if ( _old != _new.index )
      {
        old_to_new.insert( { _old, _new } );
        take_out_node( _old );
      }
    }

  }

  void substitute_node_no_restrash( node const& old_node, signal const& new_signal )
  {
    if ( is_dead( get_node( new_signal ) ) )
    {
      revive_node( get_node( new_signal ) );
    }

    for ( auto idx = 1u; idx < _storage->nodes.size(); ++idx )
    {
      if ( is_ci( idx ) || is_dead( idx ) )
        continue; /* ignore CIs and dead nodes */

      replace_in_node_no_restrash( idx, old_node, new_signal );
    }

    /* check outputs */
    replace_in_outputs( old_node, new_signal );

    /* recursively reset old node */
    if ( old_node != new_signal.index )
    {
      take_out_node( old_node );
    }

  }

  void substitute_nodes( std::list<std::pair<node, signal>> substitutions )
  {
    auto clean_substitutions = [&]( node const& n ) {
      substitutions.erase( std::remove_if( std::begin( substitutions ), std::end( substitutions ),
                                           [&]( auto const& s ) {
                                             if ( s.first == n )
                                             {
                                               node const nn = get_node( s.second );
                                               if ( is_dead( nn ) )
                                                 return true;

                                               /* deref fanout_size of the node */
                                               if ( fanout_size( nn ) > 0 )
                                               {
                                                 decr_fanout_size( nn );
                                               }
                                               /* remove the node if it's fanout_size becomes 0 */
                                               if ( fanout_size( nn ) == 0 )
                                               {
                                                 take_out_node( nn );
                                               }
                                               /* remove substitution from list */
                                               return true;
                                             }
                                             return false; /* keep */
                                           } ),
                           std::end( substitutions ) );
    };

    /* register event to delete substitutions if their light-hand side
       nodes get deleted */
    auto clean_sub_event = _events->register_delete_event( clean_substitutions );

    /* increment fanout_size of all signals to be used in
       substitutions to ensure that they will not be deleted */
    for ( const auto& s : substitutions )
    {
      incr_fanout_size( get_node( s.second ) );
    }

    while ( !substitutions.empty() )
    {
      auto const [old_node, new_signal] = substitutions.front();
      substitutions.pop_front();

      for ( auto index = 1u; index < _storage->nodes.size(); ++index )
      {
        /* skip CIs and dead nodes */
        if ( is_ci( index ) || is_dead( index ) )
          continue;

        /* skip nodes that will be deleted */
        if ( std::find_if( std::begin( substitutions ), std::end( substitutions ),
                           [&index]( auto s ) { return s.first == index; } ) != std::end( substitutions ) )
          continue;

        /* replace in node */
        if ( const auto repl = replace_in_node( index, old_node, new_signal ); repl )
        {
          incr_fanout_size( get_node( repl->second ) );
          substitutions.emplace_back( *repl );
        }
      }

      /* replace in outputs */
      replace_in_outputs( old_node, new_signal );

      /* replace in substitutions */
      for ( auto& s : substitutions )
      {
        if ( get_node( s.second ) == old_node )
        {
          s.second = is_complemented( s.second ) ? !new_signal : new_signal;
          incr_fanout_size( get_node( new_signal ) );
        }
      }

      /* finally remove the node: note that we never decrement the
         fanout_size of the old_node. instead, we remove the node and
         reset its fanout_size to 0 knowing that it must be 0 after
         substituting all references. */
      assert( !is_dead( old_node ) );
      take_out_node( old_node );

      /* decrement fanout_size when released from substitution list */
      decr_fanout_size( get_node( new_signal ) );
    }

    _events->release_delete_event( clean_sub_event );
  }
#pragma endregion restructuring

#pragma region structural properties
  size_t size() const
  {
    return static_cast<uint32_t>( _storage->nodes.size() );
  }

  size_t num_cis() const
  {
    return static_cast<uint32_t>( _storage->inputs.size() );
  }

  size_t num_cos() const
  {
    return static_cast<uint32_t>( _storage->outputs.size() );
  }

  size_t num_pis() const
  {
    return static_cast<uint32_t>( _storage->inputs.size() );
  }

  size_t num_pos() const
  {
    return static_cast<uint32_t>( _storage->outputs.size() );
  }

  size_t num_gates() const
  {
    return static_cast<uint32_t>( _storage->hash.size() );
  }

  size_t fanin_size( node const& n ) const
  {
    return static_cast<uint32_t>( _storage->nodes[n].children.size() );
  }

  node get_children( node const& n, uint32_t idx ) const
  {
    return static_cast<uint32_t>( _storage->nodes[n].children[idx].index );
  }

  size_t fanout_size( node const& n ) const
  {
    return _storage->nodes[n].nfos & UINT32_C( 0x7FFFFFFF );
  }

  size_t incr_fanout_size( node const& n ) const
  {
    return _storage->nodes[n].nfos++ & UINT32_C( 0x7FFFFFFF );
  }

  size_t decr_fanout_size( node const& n ) const
  {
    return --_storage->nodes[n].nfos & UINT32_C( 0x7FFFFFFF );
  }

#pragma endregion structural properties

#pragma region functional properties
  kitty::dynamic_truth_table node_function( const node& n ) const
  {
    return _storage->data.cache[_storage->nodes[n].func];
  }
#pragma endregion functional properties

#pragma region simulation properties

  template<typename Iterator>
  iterates_over_t<Iterator, bool>
  compute( node const& n, Iterator begin, Iterator end ) const
  {
    (void)end;
    uint32_t child{ 0 };
    uint32_t index{ 0 };
    while ( begin != end )
    {
      index <<= 1;
      index ^= *begin++ ? 1 : 0;
      if( is_complemented(_storage->nodes[n].children[child]) )
        index ^= 1;
      child++;
    }
    return kitty::get_bit( _storage->data.cache[_storage->nodes[n].func], index );
  }

    template<typename Iterator>
    iterates_over_truth_table_t<Iterator>
    compute( node const& n, Iterator begin, Iterator end ) const
    {
      (void)end;

      assert( n != 0 && !is_ci( n ) );

      const auto nfanin = _storage->nodes[n].children.size();

      std::vector<typename std::iterator_traits<Iterator>::value_type> tts( begin, end );

      //assert( nfanin != 0 );
      assert( tts.size() == nfanin );

      /* resulting truth table has the same size as any of the children */

      std::vector<aig_network::signal> children;
      
      int i{0};
      foreach_fanin( n, [&]( auto const& fi ) {
        aig_network::signal i_signal = {_aig.pi_at(i), is_complemented(fi) };//is_complemented(fi) ? !_aig.pi_at() : _storage->nodes[get_node(fi)].twin;
        children.push_back( i_signal );
        i++;
      } );

      unordered_node_map<typename std::iterator_traits<Iterator>::value_type, aig_network> node_to_tt( _aig );
      auto res = compute_rec( _aig.get_node(_storage->nodes[n].twin), children, tts, node_to_tt );
      if( _aig.is_complemented(_storage->nodes[n].twin) )
        res = ~res;

      return res;
    }

    template<typename Iterator>
    void compute( node const& n, kitty::partial_truth_table& result, Iterator begin, Iterator end ) const
    {
      static_assert( iterates_over_v<Iterator, kitty::partial_truth_table>, "begin and end have to iterate over partial_truth_tables" );

      (void)end;

      const auto nfanin = _storage->nodes[n].children.size();
      std::vector<typename std::iterator_traits<Iterator>::value_type> tts( begin, end );

      assert( nfanin != 0 );
      assert( tts.size() == nfanin );

      /* resulting truth table has the same size as any of the children */

      std::vector<aig_network::signal> children;
      
      int i{0};
      foreach_fanin( n, [&]( auto const& fi ) {
        aig_network::signal i_signal = {_aig.pi_at(i), is_complemented(fi) };
        children.push_back( i_signal );
        i++;
      } );
      //printf("node %d has twin %d\n", n, _storage->nodes[n].twin.index );

      unordered_node_map<typename std::iterator_traits<Iterator>::value_type, aig_network> node_to_tt( _aig );
      result = compute_rec( _storage->nodes[n].twin.index, children, tts, node_to_tt );

      if( _storage->nodes[n].twin.complement )
      {
        result = ~result;
      }
    }

    template<class TT, typename Iterator>
    void compute( node const& n, TT& result, Iterator begin, Iterator end ) const
    {
      static_assert( iterates_over_v<Iterator, TT>, "begin and end have to iterate over TT" );

      (void)end;

      const auto nfanin = _storage->nodes[n].children.size();
      std::vector<typename std::iterator_traits<Iterator>::value_type> tts( begin, end );

      assert( nfanin != 0 );
      assert( tts.size() == nfanin );

      /* resulting truth table has the same size as any of the children */

      std::vector<aig_network::signal> children;
      
      int i{0};
      foreach_fanin( n, [&]( auto const& fi ) {
        aig_network::signal i_signal = {_aig.pi_at(i), is_complemented(fi) };
        children.push_back( i_signal );
        i++;
      } );
      //printf("node %d has twin %d\n", n, _storage->nodes[n].twin.index );

      unordered_node_map<typename std::iterator_traits<Iterator>::value_type, aig_network> node_to_tt( _aig );
      result = compute_rec( _storage->nodes[n].twin.index, children, tts, node_to_tt );

      if( _storage->nodes[n].twin.complement )
      {
        result = ~result;
      }
    }

    template<typename TT>
    TT compute( node n, std::vector<TT> const & tts ) const
    {
      const auto nfanin = _storage->nodes[n].children.size();
      assert( nfanin == tts.size() );

      std::vector<aig_network::signal> children;
      int i{0};
      foreach_fanin( n, [&]( auto const& fi ) {
        aig_network::signal i_signal = {_aig.pi_at(i), is_complemented(fi) };//is_complemented(fi) ? !_aig.pi_at() : _storage->nodes[get_node(fi)].twin;
        children.push_back( i_signal );
        i++;
      } );

      unordered_node_map<TT, aig_network> node_to_tt( _aig );
      TT res = compute_rec( _storage->nodes[n].twin.index, children, tts, node_to_tt );
      if( _storage->nodes[n].twin.complement )
        res = ~res;

      return res;
    }

    template<typename TT>
    TT compute_rec( aig_network::node i_node, std::vector<aig_network::signal> const& children, std::vector<TT> const & tts, unordered_node_map<TT, aig_network>& node_to_tt ) const
    {
      if( node_to_tt.has( i_node ) )
      {
        return node_to_tt[i_node];
      }
      auto const& i_gate = _aig._storage->nodes[i_node];
      TT res = tts[0].construct();
      if( _aig.is_constant(i_node) )
      {
        //printf("CONSTANT IN AIG\n");
        return res;
      }

      if( _aig.is_pi(i_node) )
      {
        return _aig.is_complemented(children[_aig.pi_index(i_node)]) ? ~tts[_aig.pi_index(i_node)] : tts[_aig.pi_index(i_node)];
      }
      else
      {
        aig_network::signal const & a = i_gate.children[0];
        aig_network::signal const & b = i_gate.children[1];
        TT sim_a = a.complement ? ~compute_rec( a.index, children, tts, node_to_tt ) : compute_rec( a.index, children, tts, node_to_tt );
        TT sim_b = b.complement ? ~compute_rec( b.index, children, tts, node_to_tt ) : compute_rec( b.index, children, tts, node_to_tt );
        res =  sim_a & sim_b;
        node_to_tt[i_node] = res;
        return res;
      }
    }

    void print_aig( signal f ) const
    {
      node n = get_node(f);
      const auto nfanin = _storage->nodes[n].children.size();

      std::vector<aig_network::signal> children;
      int i{0};
      foreach_fanin( n, [&]( auto const& fi ) {
        aig_network::signal i_signal = {_aig.pi_at(i), is_complemented(fi) };//is_complemented(fi) ? !_aig.pi_at() : _storage->nodes[get_node(fi)].twin;
        children.push_back( i_signal );
        i++;
      } );
      print_aig_rec( _aig.get_node(_storage->nodes[n].twin), children );

      if( _storage->nodes[n].twin.complement )
        printf(" invert\n");
      else
        printf(" don't invert\n");
    }

    uint32_t num_aig_nodes( node n ) const
    {
      const auto nfanin = _storage->nodes[n].children.size();

      std::set<node> nodes_set;

      std::vector<aig_network::node> children;
      int i{0};
      foreach_fanin( n, [&]( auto const& fi ) {
        children.push_back( _aig.pi_at(i) );
        i++;
      } );
      return num_aig_nodes_rec( _aig.get_node(_storage->nodes[n].twin), children, nodes_set );

    }

    uint32_t num_aig_nodes_rec( aig_network::node i_node, std::vector<aig_network::node> const& children, std::set<node>& nodes_set ) const
    {
      auto const& i_gate = _aig._storage->nodes[i_node];
      if( _aig.is_constant(i_node) )
      {
        return 0;
      }

      if( _aig.is_pi(i_node) )
      {
        return 0;
      }
      if( nodes_set.find(i_node) != nodes_set.end() )
      {
        return 0;
      }
      else
      {
        aig_network::signal const & a = i_gate.children[0];
        aig_network::signal const & b = i_gate.children[1];
        uint32_t na = num_aig_nodes_rec( _aig.get_node(a), children, nodes_set );
        uint32_t nb = num_aig_nodes_rec( _aig.get_node(b), children, nodes_set );
        nodes_set.insert( i_node );
        return 1 + na + nb;
      }
    }

    void print_aig_rec( aig_network::node i_node, std::vector<aig_network::signal> const& children ) const
    {
      auto const& i_gate = _aig._storage->nodes[i_node];
      if( _aig.is_constant(i_node) )
      {
        printf("[%d=%d]", i_node, 0 );
        return;
      }

      if( _aig.is_pi(i_node) )
      {
        printf( "[%d = %c%d]", i_node, children[_aig.pi_index(i_node)].complement ? '!' : ' ', children[_aig.pi_index(i_node)].index );
      }
      else
      {
        aig_network::signal const & a = i_gate.children[0];
        aig_network::signal const & b = i_gate.children[1];
        print_aig_rec( _aig.get_node(a), children );
        print_aig_rec( _aig.get_node(b), children );
        printf("[%d=(%c%d, %c%d)]", i_node, _aig.is_complemented(a) ? '!' : ' ', a.index, _aig.is_complemented(b) ? '!' : ' ', b.index );
        return;
      }
    }
  #pragma endregion simulation properties

  #pragma region application specific value
    void clear_values() const
    {
      std::for_each( _storage->nodes.begin(), _storage->nodes.end(), []( auto& n ) { n.value = 0; } );
    }

    uint32_t value( node const& n ) const
    {
      return _storage->nodes[n].value;
    }

    void set_value( node const& n, uint32_t v ) const
    {
      _storage->nodes[n].value = v;
    }

    uint32_t incr_value( node const& n ) const
    {
      return _storage->nodes[n].value++;
    }

    uint32_t decr_value( node const& n ) const
    {
      return --_storage->nodes[n].value;
    }

    uint32_t get_function_id( node const& n ) const
    {
      return _storage->nodes[n].func;
    }

    void set_library( std::vector<gate> const& library )
    {
      _library = library;
    }

    void add_binding( node const& n, int gate_id )
    {
      _storage->nodes[n].binding = gate_id;
    }

    bool add_binding_with_check( node const& n, uint32_t gate_id )
    {
      assert( gate_id < _library.size() );

      auto const& binding = _library[gate_id];

      if ( node_function( n ) == binding.function )
      {
        _storage->nodes[n].binding = gate_id;
        return true;
      }
      return false;
    }

    void remove_binding( node const& n ) const
    {
      _storage->nodes[n].binding = -1;
    }

    const gate& get_binding( node const& n ) const
    {
      return _library[_storage->nodes[n].binding];
    }

    double get_area( node const& n ) const
    {
      if( has_binding( n ) )
        return _library[_storage->nodes[n].binding].area;
      else
        return 1.0;
    }

    bool has_binding( node const& n ) const
    {
      return _storage->nodes[n].binding >= 0;
    }

    unsigned int get_binding_index( node const& n ) const
    {
      return _storage->nodes[n].binding;
    }

    const std::vector<gate>& get_library() const
    {
      return _library;
    }

    double compute_area() const
    {
      double area = 0;
      foreach_node( [&]( auto const& n, auto ) {
        if ( has_binding( n ) )
        {
          area += get_binding( n ).area;
        }
        else
        {
          if( !is_pi( n ) && !is_constant( n ) )
            area++;
        }
      } );

      return area;
    }

    double compute_worst_delay() const
    {
      topo_view ntk_topo{ *this };
      ntk_topo.set_library( _library );
      node_map<double, lig_network> delays( *this );
      double worst_delay = 0;

      ntk_topo.foreach_node( [&]( auto const& n, auto ) {
        if ( is_constant( n ) || is_pi( n ) )
        {
          delays[n] = 0;
          return true;
        }
        if ( has_binding( n ) )
        {
          auto const& g = get_binding( n );
          double gate_delay = 0;
          foreach_fanin( n, [&]( auto const& f, auto i ) {
            gate_delay = std::max( gate_delay, (double)( delays[f] + std::max( g.pins[i].rise_block_delay, g.pins[i].fall_block_delay ) ) );
          } );
          delays[n] = gate_delay;
          worst_delay = std::max( worst_delay, gate_delay );
        }
        else
        {
          double gate_delay = 1;
          foreach_fanin( n, [&]( auto const& f, auto i ) {
            gate_delay = std::max( gate_delay, (double)( delays[f] + 1u ) );
          } );
          delays[n] = gate_delay;
          worst_delay = std::max( worst_delay, gate_delay );
        }
        return true;
      } );

      return worst_delay;
    }

    void report_binding_stats( std::ostream& os = std::cout ) const
    {
      os << fmt::format( "[i] Report stats: area = {:>5.2f}; delay = {:>5.2f};\n", compute_area(), compute_worst_delay() );
    }

    void report_gates_usage( std::ostream& os = std::cout ) const
    {
      std::vector<uint32_t> gates_profile( _library.size(), 0u );
      std::unordered_map<uint32_t,uint32_t> gates_profile_map;

      double area = 0;
      foreach_node( [&]( auto const& n, auto ) {
        if ( has_binding( n ) )
        {
          auto const& g = get_binding( n );
          ++gates_profile[g.id];
          area += g.area;
        }
        else
        {
          if( !is_pi( n ) && !is_constant( n ) )
          {
            auto func_id = get_function_id( n );
            if( gates_profile_map.find( func_id ) == gates_profile_map.end() )
            {
              gates_profile_map[func_id]=1;
            }
            else
            {
              gates_profile_map[func_id]+=1;
            }
            area += 1.0;
          }
        }
      } );

      os << "[i] Report gates usage:\n";

      if( _library.size()>0 )
      {
        uint32_t tot_instances = 0u;
        for ( auto i = 0u; i < gates_profile.size(); ++i )
        {
          if ( gates_profile[i] > 0u )
          {
            float tot_gate_area = gates_profile[i] * _library[i].area;

            os << fmt::format( "[i] {:<25}", _library[i].name )
              << fmt::format( "\t Instance = {:>10d}", gates_profile[i] )
              << fmt::format( "\t Area = {:>12.2f}", tot_gate_area )
              << fmt::format( " {:>8.2f} %\n", tot_gate_area / area * 100 );

            tot_instances += gates_profile[i];
          }
        }

        os << fmt::format( "[i] {:<25}", "TOTAL" )
          << fmt::format( "\t Instance = {:>10d}", tot_instances )
          << fmt::format( "\t Area = {:>12.2f}   100.00 %\n", area );
      }
      else
      {
        uint32_t tot_instances = 0u;
        for ( auto& [key, value]: gates_profile_map )
        {
          float tot_gate_area = static_cast<float>(value);
          os << fmt::format( "[i] {:<25}", kitty::to_hex(_storage->data.cache[key]).c_str() )
            << fmt::format( "\t Instance = {:>10d}", value )
            << fmt::format( " {:>8.2f} %\n", tot_gate_area / area * 100 );

          tot_instances += value;
        }

        os << fmt::format( "[i] {:<25}", "TOTAL" )
          << fmt::format( "\t Instance = {:>10d}", tot_instances )
          << fmt::format( "\t Area = {:>12.2f}   100.00 %\n", area );
      }
    }


  #pragma endregion application specific value

  #pragma region visited flags

    void recursively_mark( node n )
    {
      if( is_pi(n) || is_constant(n) || is_marked(n) )
      {
        return;
      }

      foreach_fanin( n, [&]( const auto& f ) {
        recursively_mark( get_node(f) );
      } );

      set_mark( n );
    }

    void clear_marked() const
    {
      std::for_each( _storage->nodes.begin(), _storage->nodes.end(), []( auto& n ) { n.mark = 0; } );
    }

    void set_mark( node const& n ) const
    {
      _storage->nodes[n].mark = 1;
    }

    bool is_marked( node const& n ) const
    {
      return _storage->nodes[n].mark > 0;
    }

    void clear_visited() const
    {
      std::for_each( _storage->nodes.begin(), _storage->nodes.end(), []( auto& n ) { n.visited = 0; } );
    }

    uint32_t visited( node const& n ) const
    {
      return _storage->nodes[n].visited;
    }

    void set_visited( node const& n, uint32_t v ) const
    {
      _storage->nodes[n].visited = v;
    }

    uint32_t trav_id() const
    {
      return _storage->trav_id;
    }

    void incr_trav_id() const
    {
      ++_storage->trav_id;
    }


  #pragma endregion visited flags

  #pragma region general methods
    auto& events() const
    {
      return *_events;
    }
  #pragma endregion general methods

    void print()
    {
      printf("POs: ");
      foreach_po( [&]( auto s, auto i ) {
        printf("%c%d ", is_complemented(s) ? '!' : ' ', s.index );
      } );
      foreach_gate( [&]( auto n ) 
      { 
        printf("[%d=");
        foreach_fanin( n, [&]( auto const& fi ) {
          printf("%c%d ", is_complemented(fi) ? '!' : ' ' , fi.index );
        } );
        printf("]");
      } );
      printf("\n");
    } 



public:
  std::shared_ptr<storage_t> _storage;
  /* complete AIG database */
  xag_npn_resynthesis<aig_network, xag_network, xag_npn_db_kind::aig_complete> aig_resyn{};

  std::shared_ptr<network_events<base_type>> _events;
  aig_network _aig;
  std::vector<gate> _library;
  uint32_t max_num_fanins{0};

};


} // namespace mockturtle